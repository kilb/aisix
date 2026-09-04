//! First-class OpenAI-compatible **Files / Batches / Fine-tuning** surface
//! (#720, #873 §⑤).
//!
//! Before this module the only way to reach these provider APIs was the
//! opaque `/passthrough/:provider/*` tunnel — no unified id routing, no
//! token/cost attribution. These handlers expose the standard OpenAI
//! routes and solve the one problem the raw tunnel can't: **routing a
//! request whose body carries no `model`** (a batch create references a
//! previously-uploaded file id; the models are inside the JSONL lines).
//!
//! ## Routing (LiteLLM-baseline mechanism)
//!
//! Following LiteLLM's file-id encoding scheme
//! (`litellm/proxy/openai_files_endpoints/common_utils.py`), a file
//! uploaded through the gateway names its routing Model once (multipart
//! `model` field, `?model=` query, or `x-aisix-model` header); the file id
//! returned to the caller is re-encoded to embed that Model. Every later
//! call that references the id (batch create, file retrieve/delete/content,
//! fine-tuning `training_file`) decodes the id and routes automatically —
//! no per-call provider hint needed. Precedence per call, matching
//! LiteLLM's scenario order: **id-embedded model → explicit
//! body/query/header model → first accessible OpenAI-compatible model**.
//!
//! Ids minted by the gateway look like `aisix-<base64url>`; raw provider
//! ids are still accepted (routing falls back to the explicit/default
//! model), mirroring LiteLLM's tolerance on list/retrieve flows.
//!
//! ## Provider coverage
//!
//! v1 targets adapter `openai` (any OpenAI-compatible `api_base`,
//! including DeepSeek/xAI-style vendors) and `azure-openai`
//! (resource-scoped `/openai/*` routes + `api-key` header). Vertex
//! (GCS-staged) / Bedrock (S3-staged) / Anthropic native
//! `/v1/messages/batches` are different wire+storage flows — tracked
//! separately, rejected here with a clear 400.
//!
//! ## Usage / cost attribution
//!
//! Management calls emit zero-token UsageEvents (parity with
//! `/passthrough`, #699). When a **batch retrieve** first observes
//! `status == "completed"`, the gateway downloads the output JSONL once
//! (per-process dedup + a deterministic UUIDv5 `request_id` derived
//! from the batch identity — the telemetry contract requires a UUID),
//! aggregates per-line `usage`, and
//! emits real token counts grouped by the provider-billed model —
//! LiteLLM's `_handle_completed_batch` equivalent
//! (`litellm/batches/batch_utils.py`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_core::models::model::Adapter;
use aisix_core::resource::ResourceEntry;
use aisix_core::{Model, ProviderKey};
use aisix_obs::{AccessLog, UsageEvent};
use axum::body::Body;
use axum::extract::{Multipart, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use bytes::Bytes;
use serde_json::Value;

use crate::auth::AuthenticatedKey;
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::reject::AisixPath;
use crate::state::ProxyState;

/// Marker prefix for gateway-minted routed ids.
const ROUTED_ID_PREFIX: &str = "aisix-";

/// Azure OpenAI data-plane API version used for the resource-scoped
/// jobs routes (`/openai/files`, `/openai/batches`,
/// `/openai/fine_tuning/jobs`). Latest GA at authoring time; callers
/// can override per request via an explicit `api-version` query
/// parameter. Mirrors `AzureUpstreamRef::DEFAULT_API_VERSION` in
/// aisix-provider-azure-openai (kept as a local const — this crate
/// does not depend on the provider crates).
const AZURE_JOBS_API_VERSION: &str = "2024-10-21";

/// Timeout for the background output-file download of the batch cost
/// attribution task. Deliberately generous — completed batch output
/// files can be large, and the task runs detached from any caller.
const BATCH_ATTRIBUTION_TIMEOUT: Duration = Duration::from_secs(120);

// ─────────────────────────── id codec ───────────────────────────

/// Encode a provider-issued id + its routing Model into a gateway id:
/// `aisix-<base64url_nopad("<raw>;model,<display_name>")>`.
pub(crate) fn encode_routed_id(raw: &str, model: &str) -> String {
    format!(
        "{ROUTED_ID_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(format!("{raw};model,{model}"))
    )
}

/// Decode a gateway-minted id back to `(raw_provider_id, model)`.
/// Returns `None` for anything that isn't a well-formed gateway id —
/// callers treat those as raw provider ids (LiteLLM parity).
pub(crate) fn decode_routed_id(id: &str) -> Option<(String, String)> {
    let b64 = id.strip_prefix(ROUTED_ID_PREFIX)?;
    let bytes = URL_SAFE_NO_PAD.decode(b64).ok()?;
    let s = String::from_utf8(bytes).ok()?;
    // rsplit: a model display name cannot contain the separator without
    // making the encode ambiguous; raw provider ids never do.
    let (raw, model) = s.rsplit_once(";model,")?;
    if raw.is_empty() || model.is_empty() {
        return None;
    }
    Some((raw.to_string(), model.to_string()))
}

/// Charset guard for ids interpolated into upstream URL paths. A decoded
/// (attacker-suppliable) id must never smuggle path separators or query
/// metacharacters into the upstream URL. `pub(crate)`: the videos
/// surface applies the same guard to decoded upstream task ids.
pub(crate) fn require_safe_upstream_id(raw: &str) -> Result<(), ProxyError> {
    let ok = !raw.is_empty()
        && raw.len() <= 256
        // `.` / `..` are valid under the charset but are path-segment
        // aliases some upstream routers normalise — never forward them.
        && raw != "."
        && raw != ".."
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
    if ok {
        Ok(())
    } else {
        Err(ProxyError::InvalidRequest(format!(
            "malformed resource id {raw:?}"
        )))
    }
}

// ──────────────────────── target resolution ────────────────────────

/// The Model + ProviderKey a jobs request routes through.
pub(crate) struct JobTarget {
    pub model_entry: Arc<ResourceEntry<Model>>,
    pub pk_entry: Arc<ResourceEntry<ProviderKey>>,
    pub secret: String,
    pub adapter: Adapter,
    /// The ProviderKey's rendered `default_headers` plus the client headers
    /// its `forward_client_headers` allowlist admits, resolved once when the
    /// target is resolved so every round-trip on this surface (upload, poll,
    /// download) sends the same set (#1112 / #1167).
    pub extra_headers: Vec<(axum::http::HeaderName, axum::http::HeaderValue)>,
}

impl JobTarget {
    fn display_name(&self) -> &str {
        &self.model_entry.value.display_name
    }
    fn provider_label(&self) -> &str {
        self.model_entry.value.provider.as_deref().unwrap_or("")
    }
}

/// Which of this surface's two upstream protocols a ProviderKey speaks.
///
/// `adapter` names a protocol family and is only needed where no
/// vendor-specific bridge exists — and both protocols here have one, matched
/// on `provider`, so the two most common key shapes leave `adapter` unset.
/// The request path never asks for it; reading only `adapter` here therefore
/// refused keys that work fine on every other endpoint.
///
/// The two vocabularies share their wire strings by construction
/// (`Adapter::Openai` serialises to `"openai"`), so a provider name that *is*
/// a family name names that family. Anything else — a vendor name such as
/// `"deepseek"` — is still refused: whether that upstream speaks OpenAI's
/// files/batches protocol is genuinely unknown.
fn supported_adapter(pk: &ProviderKey) -> Option<Adapter> {
    match pk.adapter {
        Some(Adapter::Openai) => Some(Adapter::Openai),
        Some(Adapter::AzureOpenai) => Some(Adapter::AzureOpenai),
        Some(_) => None,
        None => match pk.provider.trim() {
            "openai" => Some(Adapter::Openai),
            "azure-openai" => Some(Adapter::AzureOpenai),
            _ => None,
        },
    }
}

/// Explicit routing hint from `?model=` / `x-aisix-model` header.
fn explicit_model(params: &HashMap<String, String>, headers: &HeaderMap) -> Option<String> {
    if let Some(m) = params.get("model").filter(|m| !m.trim().is_empty()) {
        return Some(m.trim().to_string());
    }
    headers
        .get("x-aisix-model")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
}

/// Resolve the routing target for a jobs request.
///
/// `wanted = None` falls back to the first non-routing Model the key can
/// access whose ProviderKey uses a supported adapter (LiteLLM's
/// `custom_llm_provider="openai"` default). Deterministic operation
/// should pass an explicit model — documented in the proxy docs.
pub(crate) fn resolve_target(
    snapshot: &aisix_core::AisixSnapshot,
    auth: &AuthenticatedKey,
    wanted: Option<&str>,
    client_ctx: &ClientContext,
) -> Result<JobTarget, ProxyError> {
    let model_entry = match wanted {
        Some(name) => {
            let entry = crate::model_resolve::resolve_model(snapshot, name)
                .ok_or_else(|| ProxyError::ModelNotFound(format!("model {name:?} not found")))?;
            if !auth.key().can_access(name) {
                return Err(ProxyError::ModelForbidden(format!(
                    "api key is not authorized for model {name:?}"
                )));
            }
            let m = &entry.value;
            if m.is_routing() || m.is_ensemble() || m.is_semantic() {
                return Err(ProxyError::InvalidRequest(format!(
                    "model {name:?} is a virtual router; files/batches/fine-tuning \
                     requests must route to a direct model"
                )));
            }
            entry
        }
        None => snapshot
            .models
            .entries()
            .into_iter()
            .find(|e| {
                let m = &e.value;
                !m.is_routing()
                    && !m.is_ensemble()
                    && !m.is_semantic()
                    && !m.display_name.contains('*')
                    && auth.key().can_access(&m.display_name)
                    && m.provider_key_id
                        .as_deref()
                        .and_then(|id| snapshot.provider_keys.get_by_id(id))
                        .is_some_and(|pk| supported_adapter(&pk.value).is_some())
            })
            .ok_or_else(|| {
                ProxyError::InvalidRequest(
                    "no model specified and no accessible OpenAI-compatible model found; \
                     pass `model` (query/header/form field) to select the routing model"
                        .into(),
                )
            })?,
    };

    let model = &model_entry.value;
    crate::dispatch::check_ip_access(model, &client_ctx.source_ip)?;

    let pk_entry = crate::dispatch::resolve_provider_key(snapshot, model)?;
    let adapter = supported_adapter(&pk_entry.value).ok_or_else(|| {
        ProxyError::InvalidRequest(format!(
            "model {:?} resolves to a provider key for {:?}, which the \
             files/batches/fine-tuning surface does not support yet. It supports \
             OpenAI and Azure OpenAI; set the key's `adapter` to `openai` or \
             `azure-openai` if that upstream speaks one of them",
            model.display_name, pk_entry.value.provider
        ))
    })?;
    let secret = crate::dispatch::require_api_key(&pk_entry.value, model)?.to_string();

    let extra_headers =
        aisix_gateway::resolve_extra_headers(&crate::dispatch::upstream_header_ctx(
            &pk_entry.value,
            &pk_entry.id,
            model,
            &model_entry.id,
            client_ctx,
        ));
    Ok(JobTarget {
        model_entry,
        pk_entry,
        secret,
        adapter,
        extra_headers,
    })
}

// ──────────────────────── upstream plumbing ────────────────────────

/// Build the upstream URL for a jobs path (`path` starts with `/`,
/// version-independent, e.g. `/files` or `/batches/<id>/cancel`).
///
/// `query` is the pre-filtered client query string to forward (routing
/// hints already removed).
fn upstream_url(target: &JobTarget, path: &str, query: &str) -> Result<String, ProxyError> {
    match target.adapter {
        Adapter::Openai => {
            let base = crate::dispatch::resolve_base_url(&target.pk_entry.value)?;
            let url = crate::dispatch::build_openai_url(&base, path);
            if query.is_empty() {
                Ok(url)
            } else {
                Ok(format!("{url}?{query}"))
            }
        }
        Adapter::AzureOpenai => {
            // Resource-scoped Azure route: {base}/openai{path}?api-version=…
            // (files/batches/fine-tuning are not deployment-scoped).
            let base = target
                .pk_entry
                .value
                .api_base
                .as_deref()
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .ok_or_else(|| {
                    ProxyError::InvalidRequest(format!(
                        "azure provider_key {:?} has no api_base",
                        target.pk_entry.value.display_name
                    ))
                })?
                .trim_end_matches('/')
                .to_string();
            let mut url = format!("{base}/openai{path}");
            let has_api_version = query
                .split('&')
                .any(|pair| pair.split('=').next() == Some("api-version"));
            if query.is_empty() {
                url = format!("{url}?api-version={AZURE_JOBS_API_VERSION}");
            } else if has_api_version {
                url = format!("{url}?{query}");
            } else {
                url = format!("{url}?{query}&api-version={AZURE_JOBS_API_VERSION}");
            }
            Ok(url)
        }
        // resolve_target only admits the two adapters above.
        _ => unreachable!("unsupported adapter reached upstream_url"),
    }
}

/// Client query pairs forwarded upstream — routing hints are gateway-only.
fn forwardable_query(raw_query: Option<&str>) -> String {
    raw_query
        .unwrap_or("")
        .split('&')
        .filter(|pair| {
            let key = pair.split('=').next().unwrap_or("");
            !pair.is_empty() && key != "model" && key != "provider"
        })
        .collect::<Vec<_>>()
        .join("&")
}

enum UpstreamBody {
    Empty,
    Json(Bytes),
    Multipart(reqwest::multipart::Form),
}

/// Send one upstream request with per-adapter auth, the model's E2E
/// timeout, and cooldown accounting on transport failures (parity with
/// `/passthrough`, #701).
async fn send_upstream(
    state: &ProxyState,
    target: &JobTarget,
    method: Method,
    url: &str,
    body: UpstreamBody,
    request_id: &str,
) -> Result<(StatusCode, HeaderMap, Bytes), ProxyError> {
    let client = crate::http_client::client_for(target.pk_entry.value.tls.as_ref());
    let mut builder = client.request(
        reqwest::Method::from_bytes(method.as_str().as_bytes())
            .map_err(|_| ProxyError::InvalidRequest("unsupported method".into()))?,
        url,
    );

    // Gateway-owned headers go into the map first; the operator / client set
    // is merged on top and skips any name already there.
    // `RequestBuilder::header` APPENDS on a repeat name, so building the map
    // is what keeps a colliding operator header from putting two values of
    // e.g. `x-aisix-request-id` on the wire.
    let mut headers = HeaderMap::new();
    let (auth_name, auth_value) = match target.adapter {
        Adapter::AzureOpenai => (
            axum::http::HeaderName::from_static("api-key"),
            target.secret.clone(),
        ),
        _ => (header::AUTHORIZATION, format!("Bearer {}", target.secret)),
    };
    if let Ok(v) = axum::http::HeaderValue::from_str(&auth_value) {
        headers.insert(auth_name, v);
    }
    if let Ok(v) = axum::http::HeaderValue::from_str(request_id) {
        headers.insert(axum::http::HeaderName::from_static("x-aisix-request-id"), v);
    }
    for (name, value) in &target.extra_headers {
        if !headers.contains_key(name) {
            headers.insert(name.clone(), value.clone());
        }
    }
    builder = builder.headers(headers);

    builder = match body {
        UpstreamBody::Empty => builder,
        UpstreamBody::Json(bytes) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(bytes),
        UpstreamBody::Multipart(form) => builder.multipart(form),
    };

    if let Some(d) =
        crate::routing::effective_timeouts(&target.model_entry.value, None, state.default_timeouts)
            .request
    {
        builder = builder.timeout(d);
    }

    let resp = builder
        .send()
        .await
        .map_err(|e| {
            crate::cooldown::note_failure(
                &state.runtime_status,
                &target.model_entry.id,
                target.model_entry.value.cooldown.as_ref(),
                aisix_gateway::BridgeError::Transport(aisix_gateway::transport_error_message(&e)),
            )
        })
        .map_err(ProxyError::Bridge)?;

    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| {
            crate::cooldown::note_failure(
                &state.runtime_status,
                &target.model_entry.id,
                target.model_entry.value.cooldown.as_ref(),
                aisix_gateway::BridgeError::UpstreamDecode(e.to_string()),
            )
        })
        .map_err(ProxyError::Bridge)?;

    Ok((
        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        headers,
        bytes,
    ))
}

/// Run the resolved input-guardrail chain over an opaque blob (whole-body
/// lossy text scan — the `/passthrough` precedent from #911 [6]).
async fn scan_input_blob(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    target: &JobTarget,
    blob: &[u8],
    monitor_hits: &mut Vec<aisix_core::GuardrailMonitorHit>,
) -> Result<(), ProxyError> {
    let ctx = aisix_guardrails::RequestContext {
        passthrough_route_id: "",
        model_id: &target.model_entry.id,
        mcp_server_id: "",
        api_key_id: &auth.entry.id,
        team_id: auth.key().team_id.as_deref(),
    };
    let chain = state.guardrail_index.resolve(&ctx);
    if chain.is_empty() {
        return Ok(());
    }
    let chat = aisix_gateway::ChatFormat::new(
        target.display_name(),
        vec![aisix_gateway::ChatMessage::user(
            String::from_utf8_lossy(blob).into_owned(),
        )],
    );
    let (verdict, hits) = aisix_guardrails::Guardrail::check_input_observed(&chain, &chat).await;
    monitor_hits.extend(hits);
    if let aisix_guardrails::GuardrailVerdict::Block {
        reason,
        guardrail_name,
    } = verdict
    {
        tracing::warn!(
            guardrail_hook = "input",
            model = %target.display_name(),
            reason = %reason,
            "guardrail blocked jobs request",
        );
        return Err(ProxyError::ContentFiltered(
            crate::error::guardrail_block_message("request", guardrail_name.as_deref()),
        ));
    }
    Ok(())
}

/// Output-side twin of [`scan_input_blob`].
async fn scan_output_blob(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    target: &JobTarget,
    blob: &[u8],
    monitor_hits: &mut Vec<aisix_core::GuardrailMonitorHit>,
) -> Result<(), ProxyError> {
    let ctx = aisix_guardrails::RequestContext {
        passthrough_route_id: "",
        model_id: &target.model_entry.id,
        mcp_server_id: "",
        api_key_id: &auth.entry.id,
        team_id: auth.key().team_id.as_deref(),
    };
    let chain = state.guardrail_index.resolve(&ctx);
    if chain.is_empty() {
        return Ok(());
    }
    let synth = aisix_gateway::ChatResponse {
        id: String::new(),
        model: target.display_name().to_string(),
        message: aisix_gateway::ChatMessage::assistant(String::from_utf8_lossy(blob).into_owned()),
        finish_reason: aisix_gateway::FinishReason::Stop,
        usage: aisix_gateway::UsageStats::default(),
    };
    let (verdict, hits) = aisix_guardrails::Guardrail::check_output_observed(&chain, &synth).await;
    monitor_hits.extend(hits);
    if let aisix_guardrails::GuardrailVerdict::Block {
        reason,
        guardrail_name,
    } = verdict
    {
        tracing::warn!(
            guardrail_hook = "output",
            model = %target.display_name(),
            reason = %reason,
            "guardrail blocked jobs response",
        );
        return Err(ProxyError::ContentFiltered(
            crate::error::guardrail_block_message("response", guardrail_name.as_deref()),
        ));
    }
    Ok(())
}

// ─────────────────────── response id rewrite ───────────────────────

/// Top-level id fields the gateway re-encodes on create/retrieve
/// responses so later calls route automatically. List responses are
/// forwarded untouched (LiteLLM parity — raw ids there fall back to
/// explicit/default routing on the next call).
const ID_FIELDS: &[&str] = &[
    "id",
    "input_file_id",
    "output_file_id",
    "error_file_id",
    "training_file",
    "validation_file",
];

fn rewrite_response_ids(v: &mut Value, model: &str) {
    let Some(obj) = v.as_object_mut() else { return };
    for field in ID_FIELDS {
        if let Some(Value::String(s)) = obj.get_mut(*field) {
            if !s.is_empty() && !s.starts_with(ROUTED_ID_PREFIX) {
                *s = encode_routed_id(s, model);
            }
        }
    }
    // Fine-tuning: `result_files` is an array of file ids.
    if let Some(Value::Array(files)) = obj.get_mut("result_files") {
        for f in files {
            if let Value::String(s) = f {
                if !s.is_empty() && !s.starts_with(ROUTED_ID_PREFIX) {
                    *s = encode_routed_id(s, model);
                }
            }
        }
    }
}

// ─────────────────────── telemetry plumbing ───────────────────────

/// Zero-token UsageEvent for one management call (create/list/retrieve/
/// cancel/delete). Same contract as the `/passthrough` event (#699),
/// but with the resolved Model attributed.
#[allow(clippy::too_many_arguments)]
fn emit_job_usage_event(
    state: &ProxyState,
    // The request's snapshot, loaded once by the handler (#941).
    snap: &aisix_core::AisixSnapshot,
    label: &'static str,
    request_id: &str,
    auth: &AuthenticatedKey,
    target: &JobTarget,
    status_code: u16,
    elapsed: Duration,
    client: &ClientContext,
    guardrail_monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
) {
    let mut event = UsageEvent {
        request_id: request_id.to_string(),
        occurred_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        model_id: target.model_entry.id.clone(),
        api_key_id: auth.entry.id.clone(),
        requested_model: target.display_name().to_string(),
        status_code,
        // Single-attempt endpoint: the attempt spans the whole request, so
        // the upstream figure and what the caller waited for coincide.
        upstream_latency_ms: elapsed.as_millis().min(u32::MAX as u128) as u32,
        downstream_latency_ms: elapsed.as_millis().min(u32::MAX as u128) as u32,
        inbound_protocol: "openai".to_string(),
        client_source_ip: client.source_ip.clone(),
        client_user_agent: client.user_agent.clone(),
        guardrail_monitor_hits,
        ..Default::default()
    };
    crate::usage_attr::apply_pk_telemetry(
        &mut event,
        &crate::usage_attr::ResolvedPk::resolve(snap, &target.pk_entry.id),
    );
    crate::usage_attr::apply_jwt_identity(&mut event, auth.jwt.as_ref());
    state.usage_sink.try_emit(label, event.clone());
    let exporters = crate::usage_attr::live_exporters(state, snap);
    state.otlp_fan_out.fan_out(
        &event,
        None,
        exporters.generation(),
        exporters.iter().map(|e| &*e.value),
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_access_log(
    method: &Method,
    path: &str,
    target: Option<&JobTarget>,
    api_key_id: &str,
    status: u16,
    elapsed: Duration,
    request_id: &str,
    error: Option<&ProxyError>,
) {
    let (error_kind, error) = match error {
        Some(e) => {
            let (kind, msg) = crate::attempt::access_log_error(e);
            (Some(kind), Some(msg))
        }
        None => (None, None),
    };
    AccessLog {
        method: method.as_str(),
        path,
        status,
        latency: elapsed,
        provider: target.map(|t| t.provider_label()).filter(|p| !p.is_empty()),
        model: target.map(|t| t.display_name()),
        api_key_id: Some(api_key_id),
        prompt_tokens: None,
        completion_tokens: None,
        total_tokens: None,
        request_id,
        // Files / batches / fine-tuning return resource ids the caller keeps
        // using, not a per-call response-object id — same reasoning as the
        // video-job id (#1289).
        provider_request_id: None,
        served_by_model: None,
        routing_attempt_count: None,
        routing_fallback_count: None,
        error_kind,
        error: error.as_deref(),
    }
    .emit();
}

/// Shared success/error tail for every handler: access log, metrics,
/// usage event, response header.
#[allow(clippy::too_many_arguments)]
fn finish(
    state: &ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    label: &'static str,
    method: Method,
    path: String,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    started: Instant,
    request_id: String,
    result: Result<(Response, JobTarget), ProxyError>,
    monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
) -> Response {
    let elapsed = started.elapsed();
    // `path` carries the real job/file id — bounded route template only.
    let endpoint = crate::normalize_endpoint_label(&path);
    match result {
        Ok((mut resp, target)) => {
            let status = resp.status().as_u16();
            emit_access_log(
                &method,
                &path,
                Some(&target),
                &auth.entry.id,
                status,
                elapsed,
                &request_id,
                None,
            );
            crate::request_metrics::record(
                state,
                endpoint,
                crate::request_metrics::Caller::new(auth),
                crate::request_metrics::Upstream {
                    provider: target.provider_label(),
                    model: target.display_name(),
                    ..Default::default()
                },
                status,
                elapsed,
            );
            emit_job_usage_event(
                state,
                snapshot,
                label,
                &request_id,
                auth,
                &target,
                status,
                elapsed,
                client,
                monitor_hits,
            );
            if let Ok(hv) = HeaderValue::from_str(&request_id) {
                resp.headers_mut().insert("x-aisix-request-id", hv);
            }
            resp
        }
        Err(err) => {
            let status = err.status().as_u16();
            emit_access_log(
                &method,
                &path,
                None,
                &auth.entry.id,
                status,
                elapsed,
                &request_id,
                Some(&err),
            );
            crate::request_metrics::record(
                state,
                endpoint,
                crate::request_metrics::Caller::new(auth),
                crate::request_metrics::Upstream {
                    provider: "",
                    model: label,
                    ..Default::default()
                },
                status,
                elapsed,
            );
            crate::usage_attr::emit_error_usage_event(
                state,
                snapshot,
                label,
                "openai",
                &request_id,
                "",
                &auth.entry.id,
                status,
                err.kind(),
                client,
            );
            err.into_response()
        }
    }
}

/// JSON response from upstream bytes with the routing model's id rewrite
/// applied. Non-JSON upstream bodies (error HTML etc.) relay verbatim.
fn json_response(
    status: StatusCode,
    upstream_headers: &HeaderMap,
    bytes: Bytes,
    rewrite_model: Option<&str>,
) -> Response {
    let body = match (rewrite_model, serde_json::from_slice::<Value>(&bytes).ok()) {
        (Some(model), Some(mut v)) => {
            rewrite_response_ids(&mut v, model);
            Bytes::from(serde_json::to_vec(&v).unwrap_or_else(|_| bytes.to_vec()))
        }
        _ => bytes,
    };
    let mut resp = Response::builder()
        .status(status)
        .body(Body::from(body))
        .unwrap();
    let ct = upstream_headers
        .get(header::CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("application/json"));
    resp.headers_mut().insert(header::CONTENT_TYPE, ct);
    resp
}

/// Routing-model precedence shared by id-addressed calls:
/// id-embedded → explicit query/header → default.
fn routed_model_hint(
    id: &str,
    params: &HashMap<String, String>,
    headers: &HeaderMap,
) -> (String, Option<String>) {
    match decode_routed_id(id) {
        Some((raw, model)) => (raw, Some(model)),
        None => (id.to_string(), explicit_model(params, headers)),
    }
}

// ──────────────────────────── /v1/files ────────────────────────────

pub(crate) async fn create_file(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    multipart: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> Response {
    let started = Instant::now();
    let request_id = client.request_id.clone();

    // Same silent class as the body-extractor rejections #863 collected: a
    // non-multipart content-type answered axum's bare 400 with no access
    // log, metrics, or envelope.
    let mut multipart = match multipart {
        Ok(multipart) => multipart,
        Err(_) => {
            return crate::reject::reject_before_dispatch(
                &state,
                "POST",
                "/v1/files",
                &request_id,
                Some(&auth.entry.id),
                started,
                crate::reject::Envelope::OpenAi,
                ProxyError::InvalidRequest("invalid multipart form data".into()),
            );
        }
    };
    let mut monitor_hits: Vec<aisix_core::GuardrailMonitorHit> = Vec::new();
    // Loaded below, after the upload is drained — see the note in
    // `audio::multipart_dispatch` (#941 audit M2).
    let mut snapshot = None;

    let result = async {
        // Re-build the outbound multipart form, extracting the gateway-only
        // `model` routing field. Text fields (purpose, expires_after[...])
        // forward verbatim.
        let mut form = reqwest::multipart::Form::new();
        let mut form_model: Option<String> = None;
        let mut file_bytes: Option<Bytes> = None;

        while let Some(field) = multipart.next_field().await.map_err(|e| {
            crate::error::proxy_error_from_multipart(
                e,
                state.request_body_limit_for("/v1/files"),
                "malformed multipart body",
            )
        })? {
            let name = field.name().unwrap_or_default().to_string();
            if name == "model" {
                let v = field.text().await.map_err(|e| {
                    crate::error::proxy_error_from_multipart(
                        e,
                        state.request_body_limit_for("/v1/files"),
                        "malformed multipart field",
                    )
                })?;
                if !v.trim().is_empty() {
                    form_model = Some(v.trim().to_string());
                }
                continue;
            }
            if name == "file" {
                let file_name = field.file_name().unwrap_or("file").to_string();
                let content_type = field.content_type().map(str::to_string);
                let bytes = field.bytes().await.map_err(|e| {
                    crate::error::proxy_error_from_multipart(
                        e,
                        state.request_body_limit_for("/v1/files"),
                        "failed to read file field",
                    )
                })?;
                let mut part = reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(file_name);
                if let Some(ct) = content_type {
                    part = part.mime_str(&ct).map_err(|e| {
                        ProxyError::InvalidRequest(format!("invalid file content-type: {e}"))
                    })?;
                }
                form = form.part("file", part);
                file_bytes = Some(bytes);
                continue;
            }
            let v = field.text().await.map_err(|e| {
                crate::error::proxy_error_from_multipart(
                    e,
                    state.request_body_limit_for("/v1/files"),
                    "malformed multipart field",
                )
            })?;
            form = form.text(name, v);
        }

        let file_bytes = file_bytes.ok_or_else(|| {
            ProxyError::InvalidRequest("multipart body must include a `file` field".into())
        })?;

        let wanted = form_model.or_else(|| explicit_model(&params, &headers));
        let snapshot = &**snapshot.insert(state.snapshot.load());
        let target = resolve_target(snapshot, &auth, wanted.as_deref(), &client)?;

        // Batch/fine-tune input files carry end-user content — scan them
        // like any other inbound payload.
        scan_input_blob(&state, &auth, &target, &file_bytes, &mut monitor_hits).await?;
        let _reservation = crate::quota::enforce(
            &state,
            snapshot,
            &auth,
            Some(&crate::quota::ModelRateLimit::from_model(
                target.display_name(),
                &target.model_entry.id,
                &target.model_entry.value,
            )),
        )
        .await?;

        let url = upstream_url(&target, "/files", "")?;
        let (status, resp_headers, bytes) = send_upstream(
            &state,
            &target,
            Method::POST,
            &url,
            UpstreamBody::Multipart(form),
            &request_id,
        )
        .await?;
        scan_output_blob(&state, &auth, &target, &bytes, &mut monitor_hits).await?;
        let model = target.display_name().to_string();
        Ok((
            json_response(status, &resp_headers, bytes, Some(&model)),
            target,
        ))
    }
    .await;

    finish(
        &state,
        // A form that failed before the model field was read never loaded
        // one.
        &snapshot.unwrap_or_else(|| state.snapshot.load()),
        "files",
        Method::POST,
        "/v1/files".into(),
        &auth,
        &client,
        started,
        request_id,
        result,
        monitor_hits,
    )
}

pub(crate) async fn list_files(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    req: axum::extract::Request,
) -> Response {
    forward_simple(
        state,
        auth,
        client,
        params,
        headers,
        FwdSpec {
            label: "files",
            method: Method::GET,
            log_path: "/v1/files".into(),
            upstream_path: "/files".into(),
            raw_query: req.uri().query().map(str::to_string),
            body: None,
            id: None,
            rewrite_ids: false,
            relay_raw_body: false,
        },
    )
    .await
}

pub(crate) async fn get_file(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (raw, embedded) = routed_model_hint(&id, &params, &headers);
    forward_simple(
        state,
        auth,
        client,
        params,
        headers,
        FwdSpec {
            label: "files",
            method: Method::GET,
            log_path: format!("/v1/files/{id}"),
            upstream_path: format!("/files/{raw}"),
            raw_query: None,
            body: None,
            id: Some((raw, embedded)),
            rewrite_ids: true,
            relay_raw_body: false,
        },
    )
    .await
}

pub(crate) async fn delete_file(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (raw, embedded) = routed_model_hint(&id, &params, &headers);
    forward_simple(
        state,
        auth,
        client,
        params,
        headers,
        FwdSpec {
            label: "files",
            method: Method::DELETE,
            log_path: format!("/v1/files/{id}"),
            upstream_path: format!("/files/{raw}"),
            raw_query: None,
            body: None,
            id: Some((raw, embedded)),
            rewrite_ids: true,
            relay_raw_body: false,
        },
    )
    .await
}

pub(crate) async fn file_content(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (raw, embedded) = routed_model_hint(&id, &params, &headers);
    forward_simple(
        state,
        auth,
        client,
        params,
        headers,
        FwdSpec {
            label: "files",
            method: Method::GET,
            log_path: format!("/v1/files/{id}/content"),
            upstream_path: format!("/files/{raw}/content"),
            raw_query: None,
            body: None,
            id: Some((raw, embedded)),
            rewrite_ids: false,
            relay_raw_body: true,
        },
    )
    .await
}

// ─────────────────────────── /v1/batches ───────────────────────────

pub(crate) async fn create_batch(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    // Result-wrapped so an extractor-layer 413 (chunked body over the
    // cap) maps to the OpenAI envelope instead of axum's stock
    // text/plain rejection — see completions.rs.
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    let started = Instant::now();
    let body = match body {
        Ok(bytes) => bytes,
        // Answer through `reject` — see completions.rs.
        Err(rej) => {
            return crate::reject::reject_before_dispatch(
                &state,
                "POST",
                "/v1/batches",
                &client.request_id,
                Some(&auth.entry.id),
                started,
                crate::reject::Envelope::OpenAi,
                crate::error::proxy_error_from_bytes_rejection(
                    rej,
                    state.request_body_limit_for("/v1/batches"),
                ),
            );
        }
    };
    let request_id = client.request_id.clone();
    let mut monitor_hits: Vec<aisix_core::GuardrailMonitorHit> = Vec::new();

    // One snapshot for the whole request (#941) — see `embeddings`.
    let snapshot = state.snapshot.load();

    let result = async {
        let mut req_json: Value = serde_json::from_slice(&body)
            .map_err(|e| ProxyError::InvalidRequest(format!("invalid JSON body: {e}")))?;

        let input_file_id = req_json
            .get("input_file_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if input_file_id.is_empty() {
            return Err(ProxyError::InvalidRequest(
                "`input_file_id` is required".into(),
            ));
        }

        // Routing precedence (LiteLLM scenario order): file-id-embedded
        // model → explicit body `model` / query / header → default.
        let (raw_file_id, embedded) = decode_routed_id(&input_file_id)
            .map(|(raw, model)| (raw, Some(model)))
            .unwrap_or((input_file_id.clone(), None));
        require_safe_upstream_id(&raw_file_id)?;
        let body_model = req_json
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
        let wanted = embedded
            .or(body_model)
            .or_else(|| explicit_model(&params, &headers));
        let target = resolve_target(&snapshot, &auth, wanted.as_deref(), &client)?;

        // Forward the provider wire shape: raw file id, no gateway-only
        // routing fields.
        if let Some(obj) = req_json.as_object_mut() {
            obj.insert("input_file_id".into(), Value::String(raw_file_id));
            obj.remove("model");
            obj.remove("provider");
        }
        let out_body = Bytes::from(serde_json::to_vec(&req_json).unwrap_or_default());

        scan_input_blob(&state, &auth, &target, &out_body, &mut monitor_hits).await?;
        let _reservation = crate::quota::enforce(
            &state,
            &snapshot,
            &auth,
            Some(&crate::quota::ModelRateLimit::from_model(
                target.display_name(),
                &target.model_entry.id,
                &target.model_entry.value,
            )),
        )
        .await?;

        let url = upstream_url(&target, "/batches", "")?;
        let (status, resp_headers, bytes) = send_upstream(
            &state,
            &target,
            Method::POST,
            &url,
            UpstreamBody::Json(out_body),
            &request_id,
        )
        .await?;
        scan_output_blob(&state, &auth, &target, &bytes, &mut monitor_hits).await?;
        let model = target.display_name().to_string();
        Ok((
            json_response(status, &resp_headers, bytes, Some(&model)),
            target,
        ))
    }
    .await;

    finish(
        &state,
        &snapshot,
        "batches",
        Method::POST,
        "/v1/batches".into(),
        &auth,
        &client,
        started,
        request_id,
        result,
        monitor_hits,
    )
}

pub(crate) async fn get_batch(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let started = Instant::now();
    let request_id = client.request_id.clone();
    let (raw, embedded) = routed_model_hint(&id, &params, &headers);
    let mut monitor_hits: Vec<aisix_core::GuardrailMonitorHit> = Vec::new();

    // One snapshot for the whole request (#941) — see `embeddings`.
    let snapshot = state.snapshot.load();

    let result = async {
        require_safe_upstream_id(&raw)?;
        let wanted = embedded.or_else(|| explicit_model(&params, &headers));
        let target = resolve_target(&snapshot, &auth, wanted.as_deref(), &client)?;
        let _reservation = crate::quota::enforce(
            &state,
            &snapshot,
            &auth,
            Some(&crate::quota::ModelRateLimit::from_model(
                target.display_name(),
                &target.model_entry.id,
                &target.model_entry.value,
            )),
        )
        .await?;

        let url = upstream_url(&target, &format!("/batches/{raw}"), "")?;
        let (status, resp_headers, bytes) = send_upstream(
            &state,
            &target,
            Method::GET,
            &url,
            UpstreamBody::Empty,
            &request_id,
        )
        .await?;
        scan_output_blob(&state, &auth, &target, &bytes, &mut monitor_hits).await?;

        // Batch cost attribution (#720): first observation of a completed
        // batch downloads the output JSONL and emits real token usage.
        if status == StatusCode::OK {
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                maybe_attribute_batch(
                    &state,
                    &auth,
                    &target,
                    &raw,
                    &v,
                    _reservation.spend_keys(),
                    &client.user_agent,
                );
            }
        }

        let model = target.display_name().to_string();
        Ok((
            json_response(status, &resp_headers, bytes, Some(&model)),
            target,
        ))
    }
    .await;

    finish(
        &state,
        &snapshot,
        "batches",
        Method::GET,
        format!("/v1/batches/{id}"),
        &auth,
        &client,
        started,
        request_id,
        result,
        monitor_hits,
    )
}

pub(crate) async fn cancel_batch(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (raw, embedded) = routed_model_hint(&id, &params, &headers);
    forward_simple(
        state,
        auth,
        client,
        params,
        headers,
        FwdSpec {
            label: "batches",
            method: Method::POST,
            log_path: format!("/v1/batches/{id}/cancel"),
            upstream_path: format!("/batches/{raw}/cancel"),
            raw_query: None,
            body: None,
            id: Some((raw, embedded)),
            rewrite_ids: true,
            relay_raw_body: false,
        },
    )
    .await
}

pub(crate) async fn list_batches(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    req: axum::extract::Request,
) -> Response {
    forward_simple(
        state,
        auth,
        client,
        params,
        headers,
        FwdSpec {
            label: "batches",
            method: Method::GET,
            log_path: "/v1/batches".into(),
            upstream_path: "/batches".into(),
            raw_query: req.uri().query().map(str::to_string),
            body: None,
            id: None,
            rewrite_ids: false,
            relay_raw_body: false,
        },
    )
    .await
}

// ──────────────────────── /v1/fine_tuning/jobs ────────────────────────

pub(crate) async fn create_ft_job(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    // Result-wrapped so an extractor-layer 413 (chunked body over the
    // cap) maps to the OpenAI envelope instead of axum's stock
    // text/plain rejection — see completions.rs.
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    let started = Instant::now();
    let body = match body {
        Ok(bytes) => bytes,
        // Answer through `reject` — see completions.rs.
        Err(rej) => {
            return crate::reject::reject_before_dispatch(
                &state,
                "POST",
                "/v1/fine_tuning/jobs",
                &client.request_id,
                Some(&auth.entry.id),
                started,
                crate::reject::Envelope::OpenAi,
                crate::error::proxy_error_from_bytes_rejection(
                    rej,
                    state.request_body_limit_for("/v1/fine_tuning/jobs"),
                ),
            );
        }
    };
    let request_id = client.request_id.clone();
    let mut monitor_hits: Vec<aisix_core::GuardrailMonitorHit> = Vec::new();

    // One snapshot for the whole request (#941) — see `embeddings`.
    let snapshot = state.snapshot.load();

    let result = async {
        let mut req_json: Value = serde_json::from_slice(&body)
            .map_err(|e| ProxyError::InvalidRequest(format!("invalid JSON body: {e}")))?;

        // `training_file` routes the job (it was uploaded through the
        // gateway); the body's `model` is the provider's BASE model to
        // fine-tune and forwards verbatim — it is NOT a gateway Model name.
        let training_file = req_json
            .get("training_file")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if training_file.is_empty() {
            return Err(ProxyError::InvalidRequest(
                "`training_file` is required".into(),
            ));
        }
        let (raw_training, embedded) = decode_routed_id(&training_file)
            .map(|(raw, model)| (raw, Some(model)))
            .unwrap_or((training_file.clone(), None));
        require_safe_upstream_id(&raw_training)?;
        let wanted = embedded.or_else(|| explicit_model(&params, &headers));
        let target = resolve_target(&snapshot, &auth, wanted.as_deref(), &client)?;

        if let Some(obj) = req_json.as_object_mut() {
            obj.insert("training_file".into(), Value::String(raw_training));
            if let Some(vf) = obj.get("validation_file").and_then(Value::as_str) {
                if let Some((raw_vf, _)) = decode_routed_id(vf) {
                    require_safe_upstream_id(&raw_vf)?;
                    obj.insert("validation_file".into(), Value::String(raw_vf));
                }
            }
            obj.remove("provider");
        }
        let out_body = Bytes::from(serde_json::to_vec(&req_json).unwrap_or_default());

        scan_input_blob(&state, &auth, &target, &out_body, &mut monitor_hits).await?;
        let _reservation = crate::quota::enforce(
            &state,
            &snapshot,
            &auth,
            Some(&crate::quota::ModelRateLimit::from_model(
                target.display_name(),
                &target.model_entry.id,
                &target.model_entry.value,
            )),
        )
        .await?;

        let url = upstream_url(&target, "/fine_tuning/jobs", "")?;
        let (status, resp_headers, bytes) = send_upstream(
            &state,
            &target,
            Method::POST,
            &url,
            UpstreamBody::Json(out_body),
            &request_id,
        )
        .await?;
        scan_output_blob(&state, &auth, &target, &bytes, &mut monitor_hits).await?;
        let model = target.display_name().to_string();
        Ok((
            json_response(status, &resp_headers, bytes, Some(&model)),
            target,
        ))
    }
    .await;

    finish(
        &state,
        &snapshot,
        "fine_tuning",
        Method::POST,
        "/v1/fine_tuning/jobs".into(),
        &auth,
        &client,
        started,
        request_id,
        result,
        monitor_hits,
    )
}

pub(crate) async fn get_ft_job(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (raw, embedded) = routed_model_hint(&id, &params, &headers);
    forward_simple(
        state,
        auth,
        client,
        params,
        headers,
        FwdSpec {
            label: "fine_tuning",
            method: Method::GET,
            log_path: format!("/v1/fine_tuning/jobs/{id}"),
            upstream_path: format!("/fine_tuning/jobs/{raw}"),
            raw_query: None,
            body: None,
            id: Some((raw, embedded)),
            rewrite_ids: true,
            relay_raw_body: false,
        },
    )
    .await
}

pub(crate) async fn cancel_ft_job(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (raw, embedded) = routed_model_hint(&id, &params, &headers);
    forward_simple(
        state,
        auth,
        client,
        params,
        headers,
        FwdSpec {
            label: "fine_tuning",
            method: Method::POST,
            log_path: format!("/v1/fine_tuning/jobs/{id}/cancel"),
            upstream_path: format!("/fine_tuning/jobs/{raw}/cancel"),
            raw_query: None,
            body: None,
            id: Some((raw, embedded)),
            rewrite_ids: true,
            relay_raw_body: false,
        },
    )
    .await
}

pub(crate) async fn list_ft_jobs(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    req: axum::extract::Request,
) -> Response {
    forward_simple(
        state,
        auth,
        client,
        params,
        headers,
        FwdSpec {
            label: "fine_tuning",
            method: Method::GET,
            log_path: "/v1/fine_tuning/jobs".into(),
            upstream_path: "/fine_tuning/jobs".into(),
            raw_query: req.uri().query().map(str::to_string),
            body: None,
            id: None,
            rewrite_ids: false,
            relay_raw_body: false,
        },
    )
    .await
}

// ─────────────────────── shared simple forward ───────────────────────

struct FwdSpec {
    label: &'static str,
    method: Method,
    log_path: String,
    upstream_path: String,
    raw_query: Option<String>,
    body: Option<Bytes>,
    /// `(raw_id, id_embedded_model)` for id-addressed calls; the raw id
    /// is charset-validated before touching the upstream URL.
    id: Option<(String, Option<String>)>,
    rewrite_ids: bool,
    /// Relay the upstream body verbatim (file content download) instead
    /// of JSON id rewriting.
    relay_raw_body: bool,
}

async fn forward_simple(
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    params: HashMap<String, String>,
    headers: HeaderMap,
    spec: FwdSpec,
) -> Response {
    let started = Instant::now();
    let request_id = client.request_id.clone();
    let method = spec.method.clone();
    let log_path = spec.log_path.clone();
    let label = spec.label;
    let mut monitor_hits: Vec<aisix_core::GuardrailMonitorHit> = Vec::new();

    // One snapshot for the whole request (#941) — see `embeddings`.
    let snapshot = state.snapshot.load();

    let result = async {
        let embedded = match &spec.id {
            Some((raw, embedded)) => {
                require_safe_upstream_id(raw)?;
                embedded.clone()
            }
            None => None,
        };
        let wanted = embedded.or_else(|| explicit_model(&params, &headers));
        let target = resolve_target(&snapshot, &auth, wanted.as_deref(), &client)?;

        if let Some(body) = &spec.body {
            scan_input_blob(&state, &auth, &target, body, &mut monitor_hits).await?;
        }
        let _reservation = crate::quota::enforce(
            &state,
            &snapshot,
            &auth,
            Some(&crate::quota::ModelRateLimit::from_model(
                target.display_name(),
                &target.model_entry.id,
                &target.model_entry.value,
            )),
        )
        .await?;

        let query = forwardable_query(spec.raw_query.as_deref());
        let url = upstream_url(&target, &spec.upstream_path, &query)?;
        let body = match spec.body {
            Some(b) => UpstreamBody::Json(b),
            None => UpstreamBody::Empty,
        };
        let (status, resp_headers, bytes) =
            send_upstream(&state, &target, spec.method, &url, body, &request_id).await?;
        scan_output_blob(&state, &auth, &target, &bytes, &mut monitor_hits).await?;

        let resp = if spec.relay_raw_body {
            let mut resp = Response::builder()
                .status(status)
                .body(Body::from(bytes))
                .unwrap();
            if let Some(ct) = resp_headers.get(header::CONTENT_TYPE) {
                resp.headers_mut().insert(header::CONTENT_TYPE, ct.clone());
            }
            resp
        } else {
            let model = spec.rewrite_ids.then(|| target.display_name().to_string());
            json_response(status, &resp_headers, bytes, model.as_deref())
        };
        Ok((resp, target))
    }
    .await;

    finish(
        &state,
        &snapshot,
        label,
        method,
        log_path,
        &auth,
        &client,
        started,
        request_id,
        result,
        monitor_hits,
    )
}

// ─────────────────── batch completion attribution ───────────────────

/// Deterministic UUIDv5 request_id for a batch attribution event.
///
/// The `/dp/telemetry` contract requires `request_id` to be a UUID
/// (the control plane rejects the event otherwise — and with it the whole flush),
/// while attribution needs a *stable* id per (batch, model-slice) so
/// re-emission after a DP restart + re-retrieve doesn't mint a new
/// identity. UUIDv5 over the batch identity gives both.
fn batch_attribution_request_id(raw_batch_id: &str, idx: usize, multi: bool) -> String {
    let name = if multi {
        format!("aisix:batch:{raw_batch_id}:{idx}")
    } else {
        format!("aisix:batch:{raw_batch_id}")
    };
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, name.as_bytes()).to_string()
}

/// First-completed-observation hook: dedup via the process-local billed
/// set, then attribute usage in a detached task. The deterministic
/// UUIDv5 `request_id` (see [`batch_attribution_request_id`]) keeps
/// repeated emission (DP restart + re-retrieve) stable per
/// (batch, model-slice) on the control plane side.
fn maybe_attribute_batch(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    target: &JobTarget,
    raw_batch_id: &str,
    batch: &Value,
    spend_keys: Vec<String>,
    user_agent: &str,
) {
    if batch.get("status").and_then(Value::as_str) != Some("completed") {
        return;
    }
    let Some(output_file_id) = batch
        .get("output_file_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        return;
    };
    if require_safe_upstream_id(&output_file_id).is_err() {
        tracing::warn!(batch_id = %raw_batch_id, "batch output_file_id failed charset guard; skipping attribution");
        return;
    }
    if !crate::state::remember_billed_batch(
        &state.billed_batches,
        raw_batch_id,
        std::time::Instant::now(),
        crate::state::BILLED_BATCH_DEDUP_WINDOW,
    ) {
        return; // already attributed by this process
    }

    let state = state.clone();
    let api_key_id = auth.entry.id.clone();
    let jwt = auth.jwt.clone();
    let model_id = target.model_entry.id.clone();
    let display_name = target.display_name().to_string();
    let cost = target.model_entry.value.cost.clone();
    let pk_id = target.pk_entry.id.to_string();
    let secret = target.secret.clone();
    let adapter = target.adapter;
    let api_base = target.pk_entry.value.api_base.clone();
    let pk_tls = target.pk_entry.value.tls.clone();
    let raw_batch_id = raw_batch_id.to_string();
    let user_agent = user_agent.to_string();

    tokio::spawn(async move {
        if let Err(e) = attribute_batch_usage(
            &state,
            &api_key_id,
            jwt.as_ref(),
            &model_id,
            &display_name,
            cost.as_ref(),
            &pk_id,
            &secret,
            adapter,
            api_base.as_deref(),
            pk_tls.as_ref(),
            &raw_batch_id,
            &output_file_id,
            &spend_keys,
            &user_agent,
        )
        .await
        {
            // Release the guard so a later retrieve can retry the download.
            state.billed_batches.remove(&raw_batch_id);
            tracing::warn!(
                batch_id = %raw_batch_id,
                error = %e,
                "batch usage attribution failed; will retry on next retrieve",
            );
        }
    });
}

/// Download the completed batch's output JSONL and emit aggregated
/// UsageEvents (one per provider-billed model appearing in the lines).
/// LiteLLM parity: `_handle_completed_batch` → per-line usage/cost.
#[allow(clippy::too_many_arguments)]
async fn attribute_batch_usage(
    state: &ProxyState,
    api_key_id: &str,
    jwt: Option<&std::sync::Arc<crate::auth::JwtIdentity>>,
    model_id: &str,
    display_name: &str,
    cost: Option<&aisix_core::models::model::ModelCost>,
    pk_id: &str,
    secret: &str,
    adapter: Adapter,
    api_base: Option<&str>,
    tls: Option<&aisix_core::models::provider_key::ProviderKeyTls>,
    raw_batch_id: &str,
    output_file_id: &str,
    // 这次请求的花费桶键，取自 retrieve 时那次预留。批处理的钱是事后落账的，
    // 落账时预留早就没了 —— 键先拿在手上，才能把这笔钱回扣到额度上。
    spend_keys: &[String],
    // 归入 `aisix_llm_tokens_by_client_total` 的客户端分类。
    user_agent: &str,
) -> Result<(), String> {
    let url = match adapter {
        Adapter::AzureOpenai => {
            let base = api_base
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .ok_or("azure provider_key has no api_base")?
                .trim_end_matches('/');
            format!(
                "{base}/openai/files/{output_file_id}/content?api-version={AZURE_JOBS_API_VERSION}"
            )
        }
        _ => {
            let base = api_base
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .ok_or("provider_key has no api_base")?;
            crate::dispatch::build_openai_url(
                base.trim_end_matches('/'),
                &format!("/files/{output_file_id}/content"),
            )
        }
    };

    let client = crate::http_client::client_for(tls);
    let mut builder = client.get(&url).timeout(BATCH_ATTRIBUTION_TIMEOUT);
    builder = match adapter {
        Adapter::AzureOpenai => builder.header("api-key", secret),
        _ => builder.header(header::AUTHORIZATION, format!("Bearer {secret}")),
    };
    let resp = builder.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("output file download returned {}", resp.status()));
    }
    let body = resp.bytes().await.map_err(|e| e.to_string())?;

    // Aggregate per provider-billed model (`response.body.model`).
    #[derive(Default)]
    struct Agg {
        prompt: u64,
        completion: u64,
        cached: u64,
        lines: u64,
    }
    let mut per_model: std::collections::BTreeMap<String, Agg> = Default::default();
    for line in body.split(|b| *b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let resp_body = &v["response"]["body"];
        let usage = &resp_body["usage"];
        let prompt = usage["prompt_tokens"].as_u64().unwrap_or(0);
        let completion = usage["completion_tokens"].as_u64().unwrap_or(0);
        let cached = usage["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0);
        if prompt == 0 && completion == 0 {
            continue; // errored line or embeddings-shape without usage
        }
        let model = resp_body["model"].as_str().unwrap_or("").to_string();
        let agg = per_model.entry(model).or_default();
        agg.prompt += prompt;
        agg.completion += completion;
        agg.cached += cached;
        agg.lines += 1;
    }

    if per_model.is_empty() {
        tracing::info!(batch_id = %raw_batch_id, "completed batch output contained no usage lines");
        return Ok(());
    }

    let snap = state.snapshot.load();
    let pk = crate::usage_attr::ResolvedPk::resolve(&snap, pk_id);
    // 指标的 provider / upstream_model 都取这个模型行的配置 —— 与请求路径同一个
    // 来源。**不能**用输出行里上游报的模型名：`metric_model_label_pair` 只收口
    // model 那一侧，upstream 是原样透传的，而这里那个字符串来自应答内容，等于
    // 让上游决定 series 基数，而这个 recorder 没有空闲回收（#451）。
    //
    // 每片的确切模型名仍然记在事件的 `provider_model_version` 上，那是它该去的
    // 地方；多片批次在指标上合并成一条 series，总量不变。
    let row = snap.models.get_by_id(model_id);
    let provider_label = row
        .as_ref()
        .and_then(|e| e.value.provider.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let configured_upstream = row
        .as_ref()
        .and_then(|e| e.value.upstream_model())
        .unwrap_or("unknown")
        .to_string();
    // Same exporter set for every model slice of one batch.
    let exporters = crate::usage_attr::live_exporters(state, &snap);
    let multi = per_model.len() > 1;
    // 这条归因会重放：进程内的去重集合是优化不是正确性机制，重启或换实例之后
    // 再看到同一个已完成批次就会重来一遍。用量事件带确定性 id，消费端据此去重
    // ——**它**是幂等的。而下面两条不是：Prometheus 计数器和只增的花费计数器
    // 都没有那层保护，重放一次就是把这批多收一遍。
    //
    // 所以只给这两条加闸，且闸放在与计数器同一个存储里：共享存储下跨重启、跨
    // 实例都只记一次，进程内存储下只在本进程有效 —— 而那种部署里计数器本身也
    // 只在本进程里，两者同寿。TTL 取 30 天：供应商保留已完成批次的量级就是这
    // 个，之后不可能再被重取。
    //
    // 事件仍然无条件发 —— 它本来就该在重放时也到达消费端。
    let credit = state
        .limiter
        .claim_once(
            &format!("batch-spend:{raw_batch_id}"),
            std::time::Duration::from_secs(30 * 24 * 3600),
        )
        .await;
    // 先回扣额度，再发事件与指标。顺序是有意的：观察者只能看见事件和指标，
    // 把额度放在它们之后，就存在一个「指标已到、额度未到」的窗口，任何据此
    // 判断的检查都会间歇性地看错。
    //
    // 用 retrieve 时那次预留的键：预留本身早已释放（它只在准入时挡人），但键
    // 是纯字符串，事后照样能记。
    // 一处算价，两处用（额度那笔、事件那笔）。上一版把公式抄了两遍 —— 那正是
    // 两个数字会各自漂走的形状。
    //
    // 输出行里的 `prompt_tokens_details.cached_tokens` 有自己的费率
    // （`cached_input_per_1k`）：按全价输入算会多收，而事件里这个计数是照报的，
    // 于是按事件重算花费的控制面会得到另一个数字。
    let slice_cost_usd = |agg: &Agg| {
        cost.map(|c| {
            c.calculate_with_cache(
                crate::usage_attr::input_tokens_for_pricing(agg.prompt, agg.cached, 0, 0),
                agg.completion,
            )
        })
        .unwrap_or(0.0)
    };
    let billed_micro_usd: u64 = per_model
        .values()
        .map(|agg| crate::usage_attr::spend_micro_usd(slice_cost_usd(agg)))
        .fold(0u64, |a, b| a.saturating_add(b));
    if credit && billed_micro_usd > 0 && !spend_keys.is_empty() {
        state
            .limiter
            .add_tokens_post_stream_all(spend_keys, billed_micro_usd);
    }
    for (idx, (provider_model, agg)) in per_model.iter().enumerate() {
        let request_id = batch_attribution_request_id(raw_batch_id, idx, multi);
        let mut event = UsageEvent {
            request_id,
            occurred_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            model_id: model_id.to_string(),
            api_key_id: api_key_id.to_string(),
            requested_model: display_name.to_string(),
            prompt_tokens: agg.prompt.min(u32::MAX as u64) as u32,
            completion_tokens: agg.completion.min(u32::MAX as u64) as u32,
            cached_prompt_tokens: agg.cached.min(u32::MAX as u64) as u32,
            status_code: 200,
            provider_model_version: provider_model.clone(),
            cost_usd: slice_cost_usd(agg),
            inbound_protocol: "batch".to_string(),
            ..Default::default()
        };
        crate::usage_attr::apply_pk_telemetry(&mut event, &pk);
        // Attribution names the identity that observed completion — the
        // same caller the event's api_key_id already reflects.
        crate::usage_attr::apply_jwt_identity(&mut event, jwt);
        state.usage_sink.try_emit("batch", event.clone());
        state.otlp_fan_out.fan_out(
            &event,
            None,
            exporters.generation(),
            exporters.iter().map(|e| &*e.value),
        );
        // 同一笔钱也要进 Prometheus 的 token/花费族。事件只进控制面的账本，
        // 而按花费序列求和扣款的那一侧看不到它 —— 于是一个批次可以消耗巨量
        // token 而用户余额分毫不动。
        //
        // 与额度同受 `credit` 管：这是个计数器，重放一次就多收一次。
        if credit {
            let owned_caller = crate::request_metrics::Caller::from_api_key_id(&snap, api_key_id);
            crate::request_metrics::record_usage(
                state,
                "/v1/batches",
                owned_caller.as_caller(),
                crate::request_metrics::Upstream {
                    provider: &provider_label,
                    model: display_name,
                    upstream_model: &configured_upstream,
                    pk: pk.labels(),
                    ..Default::default()
                },
                crate::request_metrics::Tokens {
                    input: event.prompt_tokens,
                    output: event.completion_tokens,
                    total: event.prompt_tokens.saturating_add(event.completion_tokens),
                    spend_usd: event.cost_usd,
                    client_type: state.client_classifier.classify(user_agent),
                },
            );
        }

        tracing::info!(
            batch_id = %raw_batch_id,
            provider_model = %provider_model,
            prompt_tokens = agg.prompt,
            completion_tokens = agg.completion,
            lines = agg.lines,
            "attributed completed batch usage",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// `adapter` 是可选的，而 OpenAI / Azure OpenAI 这两种最常见的密钥形状
    /// 都不需要写它（请求路径按 `provider` 找专用桥）。这里以前只读 `adapter`，
    /// 于是这些密钥在 files/batches/fine-tuning 上被拒，而报错说的是 provider
    /// 不受支持 —— 指向了错误的字段。
    #[test]
    fn provider_name_stands_in_for_an_unset_adapter() {
        use super::supported_adapter;
        use aisix_core::models::Adapter;

        let pk = |json: serde_json::Value| -> aisix_core::ProviderKey {
            serde_json::from_value(json).expect("fixture parses")
        };
        let base = |provider: &str| {
            serde_json::json!({
                "display_name": "k",
                "secret": "sk-x",
                "provider": provider,
            })
        };

        assert_eq!(
            supported_adapter(&pk(base("openai"))),
            Some(Adapter::Openai),
            "provider: openai 不带 adapter 是标准形状，不该被拒"
        );
        assert_eq!(
            supported_adapter(&pk(base("azure-openai"))),
            Some(Adapter::AzureOpenai)
        );
        // 厂商名不是族名：对面说不说这套协议，这里真的不知道。
        assert_eq!(supported_adapter(&pk(base("deepseek"))), None);
        // 写了 adapter 就以它为准 —— 一把 provider 叫 deepseek、adapter 是
        // openai 的密钥（仓库里 provider_key 的测试用的正是这个形状）要放行。
        let mut explicit = base("deepseek");
        explicit["adapter"] = serde_json::Value::String("openai".into());
        assert_eq!(supported_adapter(&pk(explicit)), Some(Adapter::Openai));
        // 而 adapter 明确是别的族时，provider 不再兜底。
        let mut other = base("openai");
        other["adapter"] = serde_json::Value::String("anthropic".into());
        assert_eq!(
            supported_adapter(&pk(other)),
            None,
            "写明了 adapter 就该按它判，不该被 provider 名字翻回来"
        );
    }

    use super::*;
    use aisix_core::resource::ResourceEntry;
    use aisix_core::snapshot::SnapshotHandle;
    use aisix_core::{AisixSnapshot, ApiKey, Model, ProxyConfig};
    use aisix_gateway::Hub;
    use aisix_obs::{UsageEvent as ObsUsageEvent, UsageSink};
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;
    use wiremock::matchers::{method as wm_method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    const PK_A: &str = "11111111-1111-1111-1111-11111111111a";
    const PK_B: &str = "11111111-1111-1111-1111-11111111111b";

    fn openai_pk(id: &str, api_base: &str) -> ResourceEntry<aisix_core::ProviderKey> {
        let json = format!(
            r#"{{"display_name":"pk-{id}","secret":"sk-up-{id}","api_base":"{api_base}","provider":"openai","adapter":"openai"}}"#
        );
        let pk: aisix_core::ProviderKey = serde_json::from_str(&json).unwrap();
        ResourceEntry::new(id, pk, 1)
    }

    fn azure_pk(id: &str, api_base: &str) -> ResourceEntry<aisix_core::ProviderKey> {
        let json = format!(
            r#"{{"display_name":"pk-az","secret":"az-secret","api_base":"{api_base}","provider":"azure-openai","adapter":"azure-openai"}}"#
        );
        let pk: aisix_core::ProviderKey = serde_json::from_str(&json).unwrap();
        ResourceEntry::new(id, pk, 1)
    }

    fn anthropic_pk(id: &str, api_base: &str) -> ResourceEntry<aisix_core::ProviderKey> {
        let json = format!(
            r#"{{"display_name":"pk-ant","secret":"sk-ant","api_base":"{api_base}","provider":"anthropic","adapter":"anthropic"}}"#
        );
        let pk: aisix_core::ProviderKey = serde_json::from_str(&json).unwrap();
        ResourceEntry::new(id, pk, 1)
    }

    fn model(entry_id: &str, name: &str, pk_id: &str) -> ResourceEntry<Model> {
        let json = format!(
            r#"{{"display_name":"{name}","provider":"openai","model_name":"gpt-4o","provider_key_id":"{pk_id}"}}"#
        );
        let m: Model = serde_json::from_str(&json).unwrap();
        ResourceEntry::new(entry_id, m, 1)
    }

    /// 带定价的模型。不配价的话花费恒 0，任何「花费有没有被记」的断言都是
    /// 空的 —— 计数器压根不会出现，无论代码怎么写都通过。
    fn priced_model(entry_id: &str, name: &str, pk_id: &str) -> ResourceEntry<Model> {
        let json = format!(
            r#"{{"display_name":"{name}","provider":"openai","model_name":"gpt-4o","provider_key_id":"{pk_id}","cost":{{"input_per_1k":0.5,"output_per_1k":0.5}}}}"#
        );
        let m: Model = serde_json::from_str(&json).unwrap();
        ResourceEntry::new(entry_id, m, 1)
    }

    fn apikey_entry(allowed: &[&str]) -> ResourceEntry<ApiKey> {
        let json = format!(
            r#"{{"key_hash":"8b6712790a2089c67aa97a2d80022df18cc65c7814350e33baebe79aab508891","allowed_models":{}}}"#,
            serde_json::to_string(&allowed).unwrap()
        );
        let k: ApiKey = serde_json::from_str(&json).unwrap();
        ResourceEntry::new("k-1", k, 1)
    }

    fn build_app(snap: AisixSnapshot) -> axum::Router {
        let hub = Arc::new(Hub::new());
        let handle = SnapshotHandle::new(snap);
        crate::build_router(crate::ProxyState::new(handle, hub, &cfg()).without_cache())
    }

    fn build_app_with_sink(
        snap: AisixSnapshot,
    ) -> (axum::Router, tokio::sync::mpsc::Receiver<ObsUsageEvent>) {
        let (app, rx, _state) = build_app_with_sink_and_state(snap);
        (app, rx)
    }

    /// 同上，但把 state 也交出来 —— 有些断言要看它的指标，或者先在它的限流
    /// 存储上占掉一个一次性标记。
    fn build_app_with_sink_and_state(
        snap: AisixSnapshot,
    ) -> (
        axum::Router,
        tokio::sync::mpsc::Receiver<ObsUsageEvent>,
        crate::ProxyState,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel::<ObsUsageEvent>(32);
        let hub = Arc::new(Hub::new());
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        (crate::build_router(state.clone()), rx, state)
    }

    // ---- id codec ----

    #[test]
    fn batch_attribution_request_id_is_a_stable_uuid() {
        let single = batch_attribution_request_id("batch_1", 0, false);
        uuid::Uuid::parse_str(&single).expect("must be a UUID");
        assert_eq!(single, batch_attribution_request_id("batch_1", 0, false));
        // Multi-model slices and different batches get distinct ids.
        let s0 = batch_attribution_request_id("batch_1", 0, true);
        let s1 = batch_attribution_request_id("batch_1", 1, true);
        assert_ne!(s0, s1);
        assert_ne!(single, s0);
        assert_ne!(single, batch_attribution_request_id("batch_2", 0, false));
    }

    #[test]
    fn routed_id_roundtrip() {
        let id = encode_routed_id("file-abc123", "jobs-a");
        assert!(id.starts_with(ROUTED_ID_PREFIX));
        assert_eq!(
            decode_routed_id(&id),
            Some(("file-abc123".to_string(), "jobs-a".to_string()))
        );
        // Raw provider ids are not gateway ids.
        assert_eq!(decode_routed_id("file-abc123"), None);
        assert_eq!(decode_routed_id("batch_xyz"), None);
        // Tampered payloads fail closed.
        assert_eq!(decode_routed_id("aisix-!!!!"), None);
    }

    #[test]
    fn safe_upstream_id_rejects_path_and_query_injection() {
        assert!(require_safe_upstream_id("file-abc_1.2:3").is_ok());
        for bad in [
            "",
            "file/../../admin",
            "file?x=1",
            "file#frag",
            "file abc",
            "file&x=1",
            // Bare path-segment aliases: valid charset, but some
            // upstream routers normalise them into the parent path.
            ".",
            "..",
        ] {
            assert!(
                require_safe_upstream_id(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    // ---- files ----

    fn multipart_body(boundary: &str, model: Option<&str>) -> Vec<u8> {
        let mut b = Vec::new();
        if let Some(m) = model {
            b.extend_from_slice(
                format!(
                    "--{boundary}\r\ncontent-disposition: form-data; name=\"model\"\r\n\r\n{m}\r\n"
                )
                .as_bytes(),
            );
        }
        b.extend_from_slice(
            format!(
                "--{boundary}\r\ncontent-disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n"
            )
            .as_bytes(),
        );
        b.extend_from_slice(
            format!(
                "--{boundary}\r\ncontent-disposition: form-data; name=\"file\"; filename=\"input.jsonl\"\r\ncontent-type: application/jsonl\r\n\r\n{{\"custom_id\":\"r1\"}}\r\n"
            )
            .as_bytes(),
        );
        b.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        b
    }

    #[tokio::test]
    async fn create_file_routes_by_form_model_and_encodes_response_id() {
        let upstream = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(path("/v1/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "file-abc",
                "object": "file",
                "purpose": "batch",
                "filename": "input.jsonl"
            })))
            .mount(&upstream)
            .await;

        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.models.insert(model("m-a", "jobs-a", PK_A));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let app = build_app(snap);

        let boundary = "XBOUNDARYX";
        let req = Request::builder()
            .method("POST")
            .uri("/v1/files")
            .header("authorization", "Bearer sk-caller")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(axum::body::Body::from(multipart_body(
                boundary,
                Some("jobs-a"),
            )))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let got_id = v["id"].as_str().unwrap();
        assert_eq!(
            decode_routed_id(got_id),
            Some(("file-abc".to_string(), "jobs-a".to_string())),
            "response id must embed the routing model"
        );

        // The gateway-only `model` form field must NOT reach the provider.
        let received = upstream.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let body = String::from_utf8_lossy(&received[0].body);
        assert!(
            !body.contains("name=\"model\""),
            "routing field must be stripped from the upstream form; got: {body}"
        );
        assert!(body.contains("name=\"purpose\""), "purpose must forward");
        assert!(
            body.contains("filename=\"input.jsonl\""),
            "file part must forward"
        );
        // Upstream auth is the PK secret.
        assert_eq!(
            received[0]
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some(format!("Bearer sk-up-{PK_A}").as_str())
        );
    }

    #[tokio::test]
    async fn file_content_relays_bytes_with_query_model_routing() {
        let upstream = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(path("/v1/files/file-raw/content"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("{\"custom_id\":\"r1\"}\n", "application/jsonl"),
            )
            .mount(&upstream)
            .await;

        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.models.insert(model("m-a", "jobs-a", PK_A));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let app = build_app(snap);

        // Raw provider id + explicit ?model= fallback routing.
        let req = Request::builder()
            .method("GET")
            .uri("/v1/files/file-raw/content?model=jobs-a")
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        assert_eq!(&bytes[..], b"{\"custom_id\":\"r1\"}\n");
    }

    // ---- batches ----

    #[tokio::test]
    async fn create_batch_decodes_input_file_id_and_routes_to_embedded_model() {
        // Two providers; the encoded file id names model B — the request
        // must land on upstream B with the RAW file id and no gateway
        // routing fields.
        let upstream_a = MockServer::start().await;
        let upstream_b = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(path("/v1/batches"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "batch_777",
                "object": "batch",
                "status": "validating",
                "input_file_id": "file-realB"
            })))
            .mount(&upstream_b)
            .await;

        let snap = AisixSnapshot::new();
        snap.provider_keys
            .insert(openai_pk(PK_A, &upstream_a.uri()));
        snap.provider_keys
            .insert(openai_pk(PK_B, &upstream_b.uri()));
        snap.models.insert(model("m-a", "jobs-a", PK_A));
        snap.models.insert(model("m-b", "jobs-b", PK_B));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let app = build_app(snap);

        let encoded = encode_routed_id("file-realB", "jobs-b");
        let body = serde_json::json!({
            "input_file_id": encoded,
            "endpoint": "/v1/chat/completions",
            "completion_window": "24h"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/batches")
            .header("authorization", "Bearer sk-caller")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let received = upstream_b.received_requests().await.unwrap();
        assert_eq!(received.len(), 1, "request must land on upstream B");
        let sent: Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(sent["input_file_id"], "file-realB");
        assert!(sent.get("model").is_none(), "routing hints must not leak");

        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            decode_routed_id(v["id"].as_str().unwrap()),
            Some(("batch_777".to_string(), "jobs-b".to_string()))
        );
        assert_eq!(
            decode_routed_id(v["input_file_id"].as_str().unwrap()),
            Some(("file-realB".to_string(), "jobs-b".to_string()))
        );
        assert!(
            upstream_a.received_requests().await.unwrap().is_empty(),
            "upstream A must not be touched"
        );
    }

    /// 批处理输出行里从提示缓存读回来的 token 要按缓存费率计价。
    ///
    /// 这些计数是聚合了、也报进事件的（`cached_prompt_tokens`），只有价钱那步
    /// 用的是不含缓存的 `calculate`。按事件重算花费的控制面因此会得到另一个
    /// 数字 —— 同一批账，两个平面两个价。
    ///
    /// 一行：10 输入（其中 4 来自缓存）+ 5 输出。输入 $10/1k、缓存读 $1/1k、
    /// 输出 $20/1k：应为 6*0.01 + 4*0.001 + 5*0.02 = $0.164。按全价输入算会
    /// 得到 10*0.01 + 5*0.02 = $0.20。
    #[tokio::test]
    async fn batch_cached_tokens_are_priced_at_the_cache_rate() {
        let upstream = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(path("/v1/batches/batch_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "batch_1",
                "object": "batch",
                "status": "completed",
                "output_file_id": "file-out"
            })))
            .mount(&upstream)
            .await;
        let line = serde_json::json!({
            "id": "r",
            "response": {"status_code": 200, "body": {
                "model": "gpt-4o",
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 5,
                    "prompt_tokens_details": {"cached_tokens": 4}
                }
            }}
        });
        Mock::given(wm_method("GET"))
            .and(path("/v1/files/file-out/content"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(format!("{line}\n"), "application/jsonl"),
            )
            .mount(&upstream)
            .await;

        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        let m: Model = serde_json::from_str(&format!(
            r#"{{"display_name":"jobs-a","provider":"openai","model_name":"gpt-4o","provider_key_id":"{PK_A}","cost":{{"input_per_1k":10.0,"output_per_1k":20.0,"cached_input_per_1k":1.0}}}}"#
        ))
        .unwrap();
        snap.models.insert(ResourceEntry::new("m-a", m, 1));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let (app, mut rx, _state) = build_app_with_sink_and_state(snap);

        let encoded = encode_routed_id("batch_1", "jobs-a");
        let req = Request::builder()
            .method("GET")
            .uri(format!("/v1/batches/{encoded}"))
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);

        let mut ev = None;
        for _ in 0..40 {
            match tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await {
                Ok(Some(e)) if e.inbound_protocol == "batch" => {
                    ev = Some(e);
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        let ev = ev.expect("batch 用量事件");
        assert_eq!(ev.cached_prompt_tokens, 4, "缓存 token 没被聚合");
        assert!(
            (ev.cost_usd - 0.164).abs() < 1e-9,
            "缓存读按全价输入计了价（应为 0.164，得到 {}）",
            ev.cost_usd
        );
    }

    /// 输出行里上游报的模型名不能进指标标签：那是应答内容，等于让上游决定
    /// series 基数，而这个 recorder 没有空闲回收（#451）。标签取模型行配置里的
    /// 上游名 —— 与其它端点同一个来源 —— 所以一个多模型批次在指标上是**一条**
    /// series（总量不变），每片的确切模型名留在事件的 `provider_model_version`。
    #[tokio::test]
    async fn batch_metric_labels_come_from_config_not_from_the_output_lines() {
        let upstream = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(path("/v1/batches/batch_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "batch_1",
                "object": "batch",
                "status": "completed",
                "output_file_id": "file-out"
            })))
            .mount(&upstream)
            .await;
        // 两行，上游报了两个不同的模型名 —— 其中一个还是明显不该出现在标签里的
        // 长字符串。
        let line = |m: &str| {
            serde_json::json!({
                "id": "r",
                "response": {"status_code": 200, "body": {
                    "model": m,
                    "usage": {"prompt_tokens": 1000, "completion_tokens": 1000}
                }}
            })
            .to_string()
        };
        Mock::given(wm_method("GET"))
            .and(path("/v1/files/file-out/content"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                format!(
                    "{}\n{}\n",
                    line("gpt-4o-2024-08-06"),
                    line("whatever-the-upstream-says-x9271")
                ),
                "application/jsonl",
            ))
            .mount(&upstream)
            .await;

        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.models.insert(priced_model("m-a", "jobs-a", PK_A));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let (app, _rx, state) = build_app_with_sink_and_state(snap);

        let encoded = encode_routed_id("batch_1", "jobs-a");
        let req = Request::builder()
            .method("GET")
            .uri(format!("/v1/batches/{encoded}"))
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);

        let mut rendered = String::new();
        for _ in 0..40 {
            rendered = state.metrics.render();
            if rendered.contains(aisix_obs::metrics::M_LLM_SPEND_MICRO_USD_TOTAL) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            rendered.contains(aisix_obs::metrics::M_LLM_SPEND_MICRO_USD_TOTAL),
            "花费序列没出现，这条断言就量不到东西: {rendered}"
        );
        assert!(
            !rendered.contains("whatever-the-upstream-says-x9271"),
            "上游报的模型名进了指标标签 —— series 基数由上游决定了: {rendered}"
        );
        let series: Vec<&str> = rendered
            .lines()
            .filter(|l| l.starts_with(aisix_obs::metrics::M_LLM_SPEND_MICRO_USD_TOTAL))
            .collect();
        assert_eq!(
            series.len(),
            1,
            "多模型批次该合并成一条 series（总量不变）: {series:?}"
        );
        // 两行各 1000+1000 token，$0.5/1k —— 整批 $2.00，序列单位是 micro-USD。
        assert!(
            series[0].ends_with(" 2000000"),
            "合并后的总额不对（应为 2_000_000 micro-USD）: {}",
            series[0]
        );
    }

    /// 批处理的花费归因会重放：进程内的去重集合是优化不是正确性机制，重启或
    /// 换实例之后再看到同一个已完成批次就会重来一遍。两类通道在重放下的表现
    /// 必须不同 —— 用量事件带确定性 id，消费端去重，所以它该照发；而只增的
    /// 花费计数器与 Prometheus 计数器没有那层保护，重放一次就是多收一遍，所以
    /// 它们必须被那个一次性标记挡住。
    ///
    /// 这里预先占掉标记来模拟「这批已经归因过了」。本地存储下标记随进程消失，
    /// 所以真实部署里这条分支只在共享存储上起作用 —— 而共享存储正是预付余额
    /// 本来就要求的形态（见 `granted_micro_usd` 的字段说明）。
    #[tokio::test]
    async fn a_replayed_batch_attribution_re_emits_the_event_but_not_the_counters() {
        let upstream = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(path("/v1/batches/batch_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "batch_1",
                "object": "batch",
                "status": "completed",
                "output_file_id": "file-out"
            })))
            .mount(&upstream)
            .await;
        let line = serde_json::json!({
            "id": "batch_req_1",
            "response": {"status_code": 200, "body": {
                "model": "gpt-4o-2024-08-06",
                "usage": {"prompt_tokens": 10, "completion_tokens": 5}
            }}
        });
        Mock::given(wm_method("GET"))
            .and(path("/v1/files/file-out/content"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(format!("{line}\n"), "application/jsonl"),
            )
            .mount(&upstream)
            .await;

        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.models.insert(priced_model("m-a", "jobs-a", PK_A));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let (app, mut rx, state) = build_app_with_sink_and_state(snap);

        // 「这批已经归因过了」：标记先被占掉。
        assert!(
            state
                .limiter
                .claim_once("batch-spend:batch_1", std::time::Duration::from_secs(600))
                .await,
            "标记该是空的"
        );

        let encoded = encode_routed_id("batch_1", "jobs-a");
        let req = Request::builder()
            .method("GET")
            .uri(format!("/v1/batches/{encoded}"))
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);

        // 事件照发 —— 消费端按确定性 id 去重，少发才是问题。
        let mut saw_batch_event = false;
        for _ in 0..40 {
            match tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await {
                Ok(Some(ev)) if ev.inbound_protocol == "batch" => {
                    saw_batch_event = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(saw_batch_event, "重放时用量事件也该发 —— 它本来就是幂等的");

        // 而计数器一动不动。
        let rendered = state.metrics.render();
        assert!(
            !rendered.contains(aisix_obs::metrics::M_LLM_SPEND_MICRO_USD_TOTAL),
            "重放把这批的钱又收了一遍: {rendered}"
        );
    }

    #[tokio::test]
    async fn completed_batch_retrieve_attributes_usage_once() {
        let upstream = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(path("/v1/batches/batch_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "batch_1",
                "object": "batch",
                "status": "completed",
                "input_file_id": "file-in",
                "output_file_id": "file-out"
            })))
            .mount(&upstream)
            .await;
        let line1 = serde_json::json!({
            "id": "batch_req_1",
            "custom_id": "r1",
            "response": {"status_code": 200, "body": {
                "model": "gpt-4o-2024-08-06",
                "usage": {"prompt_tokens": 10, "completion_tokens": 5,
                           "prompt_tokens_details": {"cached_tokens": 2}}
            }}
        });
        let line2 = serde_json::json!({
            "id": "batch_req_2",
            "custom_id": "r2",
            "response": {"status_code": 200, "body": {
                "model": "gpt-4o-2024-08-06",
                "usage": {"prompt_tokens": 7, "completion_tokens": 3}
            }}
        });
        Mock::given(wm_method("GET"))
            .and(path("/v1/files/file-out/content"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(format!("{line1}\n{line2}\n"), "application/jsonl"),
            )
            .mount(&upstream)
            .await;

        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.models.insert(model("m-a", "jobs-a", PK_A));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let (app, mut rx) = build_app_with_sink(snap);

        let encoded = encode_routed_id("batch_1", "jobs-a");
        let mk_req = || {
            Request::builder()
                .method("GET")
                .uri(format!("/v1/batches/{encoded}"))
                .header("authorization", "Bearer sk-caller")
                .body(axum::body::Body::empty())
                .unwrap()
        };

        let resp = app.clone().oneshot(mk_req()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Response ids re-encoded for the caller.
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            decode_routed_id(v["output_file_id"].as_str().unwrap()),
            Some(("file-out".to_string(), "jobs-a".to_string()))
        );

        // Two events expected: the zero-token management event plus ONE
        // aggregated batch event from the detached attribution task.
        let mut mgmt = 0u32;
        let mut agg: Option<ObsUsageEvent> = None;
        for _ in 0..2 {
            let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("expected a usage event")
                .expect("sink closed");
            if ev.inbound_protocol == "batch" {
                agg = Some(ev);
            } else {
                mgmt += 1;
            }
        }
        assert_eq!(mgmt, 1);
        let agg = agg.expect("aggregated batch event must be emitted");
        // The control plane accepts any visible-ASCII request_id since
        // #1288, so this is no longer a wire constraint — but a
        // synthesised batch id has no caller behind it to correlate with,
        // and minting a UUID keeps it distinguishable from a caller's own
        // id in /logs. It must also stay deterministic for re-emission.
        uuid::Uuid::parse_str(&agg.request_id)
            .expect("batch attribution request_id must be a UUID");
        assert_eq!(
            agg.request_id,
            batch_attribution_request_id("batch_1", 0, false)
        );
        assert_eq!(agg.prompt_tokens, 17);
        assert_eq!(agg.completion_tokens, 8);
        assert_eq!(agg.cached_prompt_tokens, 2);
        assert_eq!(agg.provider_model_version, "gpt-4o-2024-08-06");
        assert_eq!(agg.requested_model, "jobs-a");

        // Second retrieve: management event only — the attribution is
        // process-deduped.
        let resp = app.clone().oneshot(mk_req()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("management event expected")
            .expect("sink closed");
        assert_ne!(ev.inbound_protocol, "batch");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            rx.try_recv().is_err(),
            "no second aggregated event for the same batch"
        );
    }

    // ---- fine-tuning ----

    #[tokio::test]
    async fn ft_create_routes_by_training_file_and_reencodes_ids() {
        let upstream = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(path("/v1/fine_tuning/jobs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "ftjob-9",
                "object": "fine_tuning.job",
                "model": "gpt-4o-mini-2024-07-18",
                "training_file": "file-train",
                "result_files": ["file-res"]
            })))
            .mount(&upstream)
            .await;

        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.models.insert(model("m-a", "jobs-a", PK_A));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let app = build_app(snap);

        let body = serde_json::json!({
            // The provider BASE model forwards verbatim; routing rides the
            // encoded training_file id.
            "model": "gpt-4o-mini-2024-07-18",
            "training_file": encode_routed_id("file-train", "jobs-a"),
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/fine_tuning/jobs")
            .header("authorization", "Bearer sk-caller")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let received = upstream.received_requests().await.unwrap();
        let sent: Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(sent["training_file"], "file-train");
        assert_eq!(
            sent["model"], "gpt-4o-mini-2024-07-18",
            "FT base model must forward verbatim"
        );

        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            decode_routed_id(v["id"].as_str().unwrap()),
            Some(("ftjob-9".to_string(), "jobs-a".to_string()))
        );
        assert_eq!(
            decode_routed_id(v["result_files"][0].as_str().unwrap()),
            Some(("file-res".to_string(), "jobs-a".to_string()))
        );
    }

    // ---- azure ----

    #[tokio::test]
    async fn azure_adapter_uses_api_key_header_and_resource_scoped_path() {
        let upstream = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(path("/openai/batches/batch_az"))
            .and(query_param("api-version", AZURE_JOBS_API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "batch_az",
                "object": "batch",
                "status": "in_progress"
            })))
            .mount(&upstream)
            .await;

        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(azure_pk(PK_A, &upstream.uri()));
        let m_json = format!(
            r#"{{"display_name":"jobs-az","provider":"azure-openai","model_name":"gpt-4o","provider_key_id":"{PK_A}"}}"#
        );
        let m: Model = serde_json::from_str(&m_json).unwrap();
        snap.models.insert(ResourceEntry::new("m-az", m, 1));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let app = build_app(snap);

        let encoded = encode_routed_id("batch_az", "jobs-az");
        let req = Request::builder()
            .method("GET")
            .uri(format!("/v1/batches/{encoded}"))
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let received = upstream.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(
            received[0]
                .headers
                .get("api-key")
                .and_then(|v| v.to_str().ok()),
            Some("az-secret"),
            "azure auth is the api-key header"
        );
        assert!(
            !received[0].headers.contains_key("authorization"),
            "no Bearer header on the azure wire"
        );
    }

    // ---- guards ----

    #[tokio::test]
    async fn unsupported_adapter_is_rejected_with_400() {
        let snap = AisixSnapshot::new();
        snap.provider_keys
            .insert(anthropic_pk(PK_A, "http://unused"));
        let m_json = format!(
            r#"{{"display_name":"claude","provider":"anthropic","model_name":"claude-3-5-haiku-20241022","provider_key_id":"{PK_A}"}}"#
        );
        let m: Model = serde_json::from_str(&m_json).unwrap();
        snap.models.insert(ResourceEntry::new("m-ant", m, 1));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let app = build_app(snap);

        let req = Request::builder()
            .method("GET")
            .uri("/v1/batches/batch_1?model=claude")
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let msg = v["error"]["message"].as_str().unwrap();
        // 这条消息要说清三件事，操作者才知道下一步做什么：哪个模型、对面是
        // 什么、以及可以动哪个字段。只断言「not supported」的话，一条不告诉
        // 人该改什么的消息也照过。
        assert!(msg.contains("claude"), "没说是哪个模型: {msg}");
        assert!(msg.contains("anthropic"), "没说对面是什么: {msg}");
        assert!(
            msg.contains("adapter"),
            "没指出可以动的字段 —— 判据读的是 provider key 的 `adapter`: {msg}"
        );
    }

    #[tokio::test]
    async fn model_acl_is_enforced() {
        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, "http://unused"));
        snap.models.insert(model("m-a", "jobs-a", PK_A));
        snap.apikeys.insert(apikey_entry(&["something-else"]));
        let app = build_app(snap);

        let req = Request::builder()
            .method("GET")
            .uri("/v1/batches/batch_1?model=jobs-a")
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn no_default_model_yields_clear_400() {
        // Only an anthropic model exists — the default-target scan must
        // fail with the actionable message, not a panic or 404.
        let snap = AisixSnapshot::new();
        snap.provider_keys
            .insert(anthropic_pk(PK_A, "http://unused"));
        let m_json = format!(
            r#"{{"display_name":"claude","provider":"anthropic","model_name":"claude-3-5-haiku-20241022","provider_key_id":"{PK_A}"}}"#
        );
        let m: Model = serde_json::from_str(&m_json).unwrap();
        snap.models.insert(ResourceEntry::new("m-ant", m, 1));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let app = build_app(snap);

        let req = Request::builder()
            .method("GET")
            .uri("/v1/batches")
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no model specified"),
            "got {v}"
        );
    }
}
