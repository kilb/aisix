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
//! ## Routing (the established gateway mechanism)
//!
//! Following a mainstream gateway's file-id encoding scheme, a file
//! uploaded through the gateway names its routing Model once (multipart
//! `model` field, `?model=` query, or `x-aisix-model` header); the file id
//! returned to the caller is re-encoded to embed that Model. Every later
//! call that references the id (batch create, file retrieve/delete/content,
//! fine-tuning `training_file`) decodes the id and routes automatically —
//! no per-call provider hint needed. Precedence per call, matching
//! a mainstream gateway's scenario order: **id-embedded model → explicit
//! body/query/header model → first accessible OpenAI-compatible model**.
//!
//! Ids minted by the gateway look like `aisix-<base64url>`; raw provider
//! ids are still accepted from a key with no owner (routing falls back to the
//! explicit/default model), mirroring a mainstream gateway's tolerance on
//! list/retrieve flows.
//!
//! ## Ownership
//!
//! The upstream account has one namespace, so a Provider Key shared between
//! teams would otherwise hand each team the others' files. A key with a team or
//! user ([`job_owner`]) therefore owns what it creates: the id it is handed
//! carries that owner, and an id that names anyone else, or no one, reads as an
//! id that does not exist, on every call that addresses one. Ids derived from
//! an owned resource (a batch's `output_file_id`) carry the owner because only
//! the owner can read the resource they appear on. It cannot list, because the
//! gateway has no way to attribute the upstream's list to an owner. The owner
//! is sealed into the id ([`owner_tag`]), so an id read off a log or a shared
//! link cannot be re-issued under another owner.
//!
//! A key with neither reaches the unowned ids it was handed. Where no key with
//! an owner can reach the provider key, which is a single-tenant deployment,
//! it also lists and uses the provider's raw ids, as before ownership existed.
//! Where one can, it does neither: the provider's list and raw ids would hand
//! it the owned resources ([`pk_serves_owned_keys`]).
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
//! `/passthrough`, #699). When the gateway first observes a batch that ended
//! with billable output — its own watch of a batch it created
//! ([`watch_batch`]), a retrieve, or a list — it downloads the output JSONL
//! once (per-process dedup + a deterministic UUIDv5 `request_id` derived
//! from the batch identity — the telemetry contract requires a UUID),
//! aggregates per-line `usage`, and
//! emits real token counts grouped by the provider-billed model —
//! the equivalent of a mainstream gateway's completed-batch handling. The key
//! that submitted the batch pays, not the one that happened to observe it ([`BATCH_SUBMITTER_KEY`]).

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

/// 批次提交时由网关写进上游 `metadata` 的键：这个批次是哪把密钥提交的。付钱的
/// 就是它，而不是碰巧观察到完成的那把。
///
/// 放在上游而不是网关这边：归因可能发生在提交几天之后的另一个进程里，文件模式的
/// 网关没有能跨重启的存储。调用方写的同名键在提交时被覆盖，批次建好之后上游也不
/// 让改，所以调用方伪造不了。应答里会摘掉（[`rewrite_response_ids`]）。
const BATCH_SUBMITTER_KEY: &str = "aisix_api_key_id";
/// 同上：这个批次走的是哪个模型行，按它定价。否则读批次时换一个 `?model=` 就能
/// 换一个价钱。
const BATCH_MODEL_KEY: &str = "aisix_model";
/// 上游 `metadata` 的键值对上限。调用方已经写满时不打标记，退回按观察者归因。
const BATCH_METADATA_MAX_PAIRS: usize = 16;
/// 多久还没结束就不再看了。上游最长的完成窗口之外留一天给收尾。
const BATCH_WATCH_LIMIT: Duration = Duration::from_secs(8 * 24 * 3600);

// ─────────────────────────── id codec ───────────────────────────

/// Encode a provider-issued id + its routing Model into a gateway id:
/// `aisix-<base64url_nopad("<raw>;model,<display_name>")>`, with
/// `;owner,<owner>,<tag>` after the raw id when the resource has an owner,
/// `owner` being `(owner, provider key secret)` ([`owner_tag`]).
pub(crate) fn encode_routed_id(raw: &str, model: &str, owner: Option<(&str, &str)>) -> String {
    let plain = match owner {
        Some((owner, secret)) => {
            let tag = owner_tag(secret, raw, owner);
            format!("{raw};owner,{owner},{tag};model,{model}")
        }
        None => format!("{raw};model,{model}"),
    };
    format!("{ROUTED_ID_PREFIX}{}", URL_SAFE_NO_PAD.encode(plain))
}

/// The seal on an owned id: an HMAC-SHA256 over the raw id and its owner,
/// keyed by the secret of the provider key the id was minted on. Every replica
/// serving that key checks it with nothing more to share, and a caller who
/// reads an id cannot re-issue it under another owner. Rotating that secret
/// invalidates the owned ids it sealed: their owners find them gone, as an id
/// minted anywhere else.
///
/// The seal is exactly as strong as that secret. A provider key whose upstream
/// needs no key and holds a placeholder (`EMPTY`, `sk-local`) gives no
/// protection: whoever guesses the placeholder seals ids for any owner. A
/// provider key that serves the jobs surface to keys with an owner needs a
/// secret nobody can guess.
fn owner_tag(secret: &str, raw: &str, owner: &str) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    URL_SAFE_NO_PAD.encode(ring::hmac::sign(&key, &owner_tag_message(raw, owner)))
}

fn owner_tag_message(raw: &str, owner: &str) -> Vec<u8> {
    [
        b"aisix-job-owner-v1\0",
        raw.as_bytes(),
        b"\0",
        owner.as_bytes(),
    ]
    .concat()
}

/// Whether `tag` seals `raw` for `owner` under `secret`, compared in constant
/// time.
fn owner_tag_matches(secret: &str, raw: &str, owner: &str, tag: &str) -> bool {
    let Ok(tag) = URL_SAFE_NO_PAD.decode(tag) else {
        return false;
    };
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    ring::hmac::verify(&key, &owner_tag_message(raw, owner), &tag).is_ok()
}

/// A gateway-minted id, decoded.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RoutedId {
    pub(crate) raw: String,
    pub(crate) model: String,
    pub(crate) owner: Option<String>,
    /// The owner's seal ([`owner_tag`]), as the id carries it.
    pub(crate) tag: Option<String>,
}

/// Decode a gateway-minted id. Returns `None` for anything that isn't a
/// well-formed gateway id — callers treat those as raw provider ids.
pub(crate) fn decode_routed_id(id: &str) -> Option<RoutedId> {
    let b64 = id.strip_prefix(ROUTED_ID_PREFIX)?;
    let bytes = URL_SAFE_NO_PAD.decode(b64).ok()?;
    let s = String::from_utf8(bytes).ok()?;
    // rsplit: a model display name cannot contain the separator without
    // making the encode ambiguous; raw provider ids never do.
    let (head, model) = s.rsplit_once(";model,")?;
    let (raw, owner, tag) = match head.split_once(";owner,") {
        // The tag is base64url, so the last comma ends the owner.
        Some((raw, sealed)) => match sealed.rsplit_once(',') {
            Some((owner, tag)) => (raw, Some(owner.to_string()), Some(tag.to_string())),
            None => (raw, Some(sealed.to_string()), None),
        },
        None => (head, None, None),
    };
    if raw.is_empty() || model.is_empty() || owner.as_deref() == Some("") {
        return None;
    }
    Some(RoutedId {
        raw: raw.to_string(),
        model: model.to_string(),
        owner,
        tag,
    })
}

/// Who a key's job resources belong to: its team, else its user. `None` for a
/// key with neither.
fn job_owner(auth: &AuthenticatedKey) -> Option<String> {
    let key = auth.key();
    let non_empty = |id: &Option<String>| id.clone().filter(|id| !id.is_empty());
    non_empty(&key.team_id)
        .map(|team| format!("team:{team}"))
        .or_else(|| non_empty(&key.user_id).map(|user| format!("user:{user}")))
}

/// Refuse an id the caller does not own, once its target is known. Decided on
/// the id and the configuration, with no upstream call, so the answer says
/// nothing about whether the upstream holds it. `routed` is the gateway id
/// `raw` came in, `None` for a provider's raw id.
///
/// - A key with an owner reaches only ids sealed for it on this provider key.
/// - A key without one reaches unowned ids: sealed as unowned where a key with
///   an owner can reach this provider key ([`pk_serves_owned_keys`]), and
///   unsealed ones and raw ids only where none can.
fn require_job_owner(
    auth: &AuthenticatedKey,
    snapshot: &aisix_core::AisixSnapshot,
    target: &JobTarget,
    raw: &str,
    routed: Option<&RoutedId>,
) -> Result<(), ProxyError> {
    let reachable = match (job_owner(auth), routed) {
        (Some(mine), Some(id)) => {
            id.owner.as_deref() == Some(mine.as_str())
                && id
                    .tag
                    .as_deref()
                    .is_some_and(|tag| owner_tag_matches(&target.secret, raw, &mine, tag))
        }
        (Some(_), None) => false,
        // Where owned keys share the provider key, an unowned id must carry
        // the seal: an unsealed one is anyone's to write around a raw id.
        (None, Some(id)) => match id.owner.as_deref() {
            None => !pk_serves_owned_keys(snapshot, &target.pk_entry.id),
            Some(UNOWNED_SEAL) => id
                .tag
                .as_deref()
                .is_some_and(|tag| owner_tag_matches(&target.secret, raw, UNOWNED_SEAL, tag)),
            Some(_) => false,
        },
        (None, None) => !pk_serves_owned_keys(snapshot, &target.pk_entry.id),
    };
    if reachable {
        Ok(())
    } else {
        Err(ProxyError::JobResourceNotFound(raw.to_string()))
    }
}

/// Refuse a list where the provider's would show the caller resources it does
/// not own: always to a key with an owner, and to one without where a key with
/// an owner can reach the same provider key.
fn require_job_list(
    auth: &AuthenticatedKey,
    snapshot: &aisix_core::AisixSnapshot,
    target: &JobTarget,
) -> Result<(), ProxyError> {
    if job_owner(auth).is_some() || pk_serves_owned_keys(snapshot, &target.pk_entry.id) {
        Err(ProxyError::JobListRefused)
    } else {
        Ok(())
    }
}

/// Whether a key with a team or user can reach a model on the provider key
/// `pk_id`, so that the provider's account holds owned resources.
fn pk_serves_owned_keys(snapshot: &aisix_core::AisixSnapshot, pk_id: &str) -> bool {
    provider_keys_serving_owned_keys(snapshot).contains(pk_id)
}

/// The provider keys some key with a team or user can reach a model on,
/// computed once per change of the models or keys table rather than walking
/// keys times models on every jobs request. Keyed on the tables' generations,
/// which are unique process-wide, so one cache serves every snapshot.
fn provider_keys_serving_owned_keys(
    snapshot: &aisix_core::AisixSnapshot,
) -> Arc<std::collections::HashSet<String>> {
    type Cached = Option<((u64, u64), Arc<std::collections::HashSet<String>>)>;
    static CACHE: std::sync::Mutex<Cached> = std::sync::Mutex::new(None);
    let key = (snapshot.models.generation(), snapshot.apikeys.generation());
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((cached, set)) = cache.as_ref() {
        if *cached == key {
            return Arc::clone(set);
        }
    }
    let has = |id: &Option<String>| id.as_deref().is_some_and(|id| !id.is_empty());
    let owned: Vec<_> = snapshot
        .apikeys
        .entries()
        .into_iter()
        .filter(|entry| has(&entry.value.team_id) || has(&entry.value.user_id))
        .collect();
    let mut set = std::collections::HashSet::new();
    for model in snapshot.models.entries() {
        let Some(pk) = model.value.provider_key_id.as_deref() else {
            continue;
        };
        if !set.contains(pk)
            && owned
                .iter()
                .any(|key| key.value.can_access(snapshot, &model.value.display_name))
        {
            set.insert(pk.to_string());
        }
    }
    let set = Arc::new(set);
    *cache = Some((key, Arc::clone(&set)));
    set
}

/// The owner an unowned resource is sealed for where keys with an owner share
/// the provider key: an unsealed id there would be one anyone can write around
/// a raw id. Never an owner a key can have (`team:`, `user:`).
const UNOWNED_SEAL: &str = "-";

/// The owner a key's new job resources are sealed for, with the secret that
/// seals them: its own, or [`UNOWNED_SEAL`] for a key without one on a provider
/// key that owned keys reach. `None` mints unsealed ids, as a single-tenant
/// provider key always has.
fn owner_seal<'a>(
    auth: &AuthenticatedKey,
    snapshot: &aisix_core::AisixSnapshot,
    target: &'a JobTarget,
) -> Option<(String, &'a str)> {
    let owner = job_owner(auth).or_else(|| {
        pk_serves_owned_keys(snapshot, &target.pk_entry.id).then(|| UNOWNED_SEAL.to_string())
    })?;
    Some((owner, target.secret.as_str()))
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
#[derive(Clone)]
pub(crate) struct JobTarget {
    pub model_entry: Arc<ResourceEntry<Model>>,
    pub pk_entry: Arc<ResourceEntry<ProviderKey>>,
    pub secret: String,
    pub adapter: Adapter,
    /// The ProviderKey's rendered `default_headers`, resolved once when the
    /// target is resolved so every round-trip on this surface (upload,
    /// poll, download) sends the same set (#1112).
    pub extra_headers: Vec<(axum::http::HeaderName, axum::http::HeaderValue)>,
    /// The client headers this ProviderKey's `forward_client_headers`
    /// admits, resolved from the same inbound request and reused on the
    /// same round-trips (AISIX-Cloud#1167). Kept beside `extra_headers`
    /// because it OVERWRITES its slots while `extra_headers` merges with
    /// skip-if-present — a forwarded credential is meant to displace the
    /// gateway's own.
    forwarded_client: aisix_gateway::ForwardedClientHeaders,
}

impl JobTarget {
    fn display_name(&self) -> &str {
        &self.model_entry.value.display_name
    }
    fn provider_label(&self) -> &str {
        self.model_entry.value.provider.as_deref().unwrap_or("")
    }
}

/// Which of this surface's two upstream protocols a ProviderKey speaks,
/// decided the way the request path decides it: a key that works on
/// `/v1/chat/completions` works here, and a key that cannot be dispatched
/// there is refused here too.
///
/// `Hub::dispatch_two_tier` looks the key's `provider` up among the
/// vendor-specific bridges first — the server registers `openai` and
/// `anthropic` — and falls back to the `adapter` family only when none
/// matches. So `provider: openai` speaks OpenAI whatever `adapter` says,
/// `provider: anthropic` speaks Anthropic (no files/batches protocol here),
/// and every other provider, `azure-openai` included, needs `adapter`.
/// A vendor name such as `"deepseek"` without one is refused: whether that
/// upstream speaks OpenAI's files/batches protocol is genuinely unknown.
fn supported_adapter(pk: &ProviderKey) -> Option<Adapter> {
    match pk.provider.as_str() {
        "openai" => return Some(Adapter::Openai),
        "anthropic" => return None,
        _ => {}
    }
    match pk.adapter {
        Some(Adapter::Openai) => Some(Adapter::Openai),
        Some(Adapter::AzureOpenai) => Some(Adapter::AzureOpenai),
        _ => None,
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
/// access whose ProviderKey uses a supported adapter (a mainstream
/// gateway's default). Deterministic operation
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
                .ok_or_else(|| ProxyError::ModelNotFound(name.to_string()))?;
            if !auth.key().can_access(snapshot, name) {
                return Err(ProxyError::ModelForbidden(name.to_string()));
            }
            let m = &entry.value;
            if m.is_routing() || m.is_ensemble() || m.is_semantic() {
                return Err(ProxyError::InvalidRequest(format!(
                    "model {name:?} is a virtual router; files/batches/fine-tuning \
                     requests must route to a direct model"
                )));
            }
            // The key's provider groups hold here as on every dispatch, and
            // a model outside them is refused the way a routing group with no
            // target left is.
            if !crate::routing::target_in_groups(snapshot, m, &auth.key().allowed_provider_groups) {
                return Err(crate::routing::no_target_in_groups(name));
            }
            entry
        }
        // The lowest id rather than the first the table yields: it iterates
        // in no fixed order, so "first" would be a different provider key
        // on each replica and after each restart.
        None => snapshot
            .models
            .entries()
            .into_iter()
            .filter(|e| {
                let m = &e.value;
                !m.is_routing()
                    && !m.is_ensemble()
                    && !m.is_semantic()
                    && !m.display_name.contains('*')
                    && auth.key().can_access(snapshot, &m.display_name)
                    // Never a default outside the key's provider groups.
                    && crate::routing::target_in_groups(
                        snapshot,
                        m,
                        &auth.key().allowed_provider_groups,
                    )
                    && m.provider_key_id
                        .as_deref()
                        .and_then(|id| snapshot.provider_keys.get_by_id(id))
                        .is_some_and(|pk| supported_adapter(&pk.value).is_some())
            })
            .min_by(|a, b| a.id.cmp(&b.id))
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

    let header_ctx = crate::dispatch::upstream_header_ctx(
        &pk_entry.value,
        &pk_entry.id,
        model,
        &model_entry.id,
        client_ctx,
    );
    let forwarded_client = aisix_gateway::ForwardedClientHeaders::resolve(&header_ctx);
    let extra_headers = aisix_gateway::resolve_default_headers(&header_ctx);
    Ok(JobTarget {
        model_entry,
        pk_entry,
        secret,
        adapter,
        extra_headers,
        forwarded_client,
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

/// One upstream request with per-adapter auth and the operator and client
/// headers, so every round-trip on this surface sends the same set (#1112).
fn upstream_request(
    target: &JobTarget,
    method: reqwest::Method,
    url: &str,
    request_id: &str,
) -> reqwest::RequestBuilder {
    let client =
        crate::http_client::client_for(target.pk_entry.value.upstream_connection().as_ref());
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
    // Last, and overwriting: an operator who forwards the caller's own
    // credential into the slot the gateway just filled is choosing it over
    // the gateway's, and the upstream must receive exactly one.
    target.forwarded_client.apply(&mut headers);

    client.request(method, url).headers(headers)
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
    let mut builder = upstream_request(
        target,
        reqwest::Method::from_bytes(method.as_str().as_bytes())
            .map_err(|_| ProxyError::InvalidRequest("unsupported method".into()))?,
        url,
        request_id,
    );

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

    crate::attribution::note_upstream_wait_begin();
    let sent = builder.send().await;
    if sent.is_err() {
        crate::attribution::note_upstream_wait_end(false);
    }
    let note_deployment = |status: u16| {
        crate::attempt::RoutingTelemetry::record_deployment(state, &target.model_entry.id, status)
    };
    let resp = sent
        .map_err(|e| {
            let err =
                aisix_gateway::BridgeError::Transport(aisix_gateway::transport_error_message(&e));
            note_deployment(err.http_status());
            crate::cooldown::note_failure(
                &state.runtime_status,
                &target.model_entry.id,
                target.model_entry.value.cooldown.as_ref(),
                err,
            )
        })
        .map_err(ProxyError::Bridge)?;

    let status = resp.status();
    let headers = resp.headers().clone();
    aisix_gateway::note_upstream_response(&headers);
    let bytes = resp.bytes().await;
    crate::attribution::note_upstream_wait_end(bytes.is_ok());
    // A refusal is relayed as sent, less the credentials this request sent.
    let bytes = match bytes {
        Ok(body) if !status.is_success() => Ok(aisix_gateway::scrub_upstream_error_body(&body)),
        other => other,
    };
    if let (false, Ok(body)) = (status.is_success(), &bytes) {
        aisix_gateway::note_upstream_error_body(body);
        crate::attribution::note_relayed_error_body(body);
    }
    let bytes = bytes
        .map_err(|e| {
            let err = if status.is_success() {
                aisix_gateway::upstream_body_read_error(&e)
            } else {
                aisix_gateway::BridgeError::upstream_body_unread(status.as_u16(), &e)
            };
            note_deployment(err.http_status());
            crate::cooldown::note_failure(
                &state.runtime_status,
                &target.model_entry.id,
                target.model_entry.value.cooldown.as_ref(),
                err,
            )
        })
        .map_err(ProxyError::Bridge)?;
    note_deployment(status.as_u16());

    Ok((
        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        headers,
        bytes,
    ))
}

/// One multipart field of a `/v1/files` upload, held verbatim until the file
/// body has been through the guardrail chain. The form cannot be assembled
/// during the read: a `mask` rule rewrites the file part, and `reqwest`'s
/// `Form` has no way to replace a part once added.
enum UploadField {
    Text(String, String),
    File {
        file_name: String,
        content_type: Option<String>,
        bytes: Bytes,
    },
}

/// Every string value and object key of every JSONL line that parses,
/// decoded, one per line, and for a line with an escape that does not parse,
/// its string literals as a lenient parser reads them
/// ([`crate::json_splice::lenient_string_literals`]). `None` when there is
/// neither.
fn jsonl_decoded_text(text: &str) -> Option<String> {
    let mut out = Vec::new();
    // 文件开头的 UTF-8 BOM 不属于第一行（见 `redact_jsonl_upload`）。
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(value) => json_decoded_strings(&value, &mut out),
            Err(_) if line.contains('\\') => {
                out.push(crate::json_splice::lenient_string_literals(line));
            }
            Err(_) => {}
        }
    }
    (!out.is_empty()).then(|| out.join("\n"))
}

/// The string literals of the lines with an escape that do not parse strictly,
/// as a lenient parser reads them. Those strings cannot be rewritten in place,
/// so a mask rule that matches them refuses the upload.
fn unparsed_escaped_literals(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    text.lines()
        .filter(|line| {
            line.contains('\\') && serde_json::from_str::<serde_json::Value>(line).is_err()
        })
        .map(crate::json_splice::lenient_string_literals)
        .collect()
}

/// Every string value and object key of `value`, decoded.
pub(crate) fn json_decoded_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => items.iter().for_each(|v| json_decoded_strings(v, out)),
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                out.push(k.clone());
                json_decoded_strings(v, out);
            }
        }
        _ => {}
    }
}

/// 输入审查读的文本：原样的字节之外，还有 JSONL 每行解码后的字符串。上游读
/// 的是解码后的内容，`\u0068ow to …` 这类转义、以及 Python `json.dumps` 默认
/// 把非 ASCII 全部转义成的 `\uXXXX`，在原样字节里都对不上规则。没有反斜杠就
/// 没有转义，解码后的字符串都是原文的子串，不必再查一遍。
fn inspection_text(blob: &[u8]) -> String {
    let raw = String::from_utf8_lossy(blob).into_owned();
    match raw
        .contains('\\')
        .then(|| jsonl_decoded_text(&raw))
        .flatten()
    {
        Some(decoded) => format!("{raw}\n{decoded}"),
        None => raw,
    }
}

/// 按这个目标模型的限流预留一次请求：这条面上每个转发都要占的那一格。
async fn reserve_target(
    state: &ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    auth: &AuthenticatedKey,
    target: &JobTarget,
) -> Result<aisix_ratelimit::MultiReservation, ProxyError> {
    crate::quota::enforce(
        state,
        snapshot,
        auth,
        Some(&crate::quota::ModelRateLimit::from_model(
            target.display_name(),
            &target.model_entry.id,
            &target.model_entry.value,
        )),
    )
    .await
}

/// 输入审查要调用远端审核服务时，审查之前先预留（见
/// `quota::reserve_before_costly_check`）：到了限额的 key 在任何远端调用之前
/// 就被 429 拒掉，被审查拒绝的请求也照样占掉它预留的那一格。只跑本地规则时
/// 返回 `None`，预留照旧放在审查之后。
async fn reserve_before_costly_scan(
    state: &ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    auth: &AuthenticatedKey,
    target: &JobTarget,
) -> Result<Option<aisix_ratelimit::MultiReservation>, ProxyError> {
    let ctx = aisix_guardrails::RequestContext {
        passthrough_route_id: "",
        model_id: &target.model_entry.id,
        mcp_server_id: "",
        api_key_id: &auth.entry.id,
        team_id: auth.key().team_id.as_deref(),
    };
    let chain = state.guardrail_index.resolve(&ctx);
    crate::quota::reserve_before_costly_check(&chain, reserve_target(state, snapshot, auth, target))
        .await
}

/// Run the resolved input-guardrail chain over an opaque blob (whole-body
/// lossy text scan — the `/passthrough` precedent from #911 [6]).
#[allow(clippy::too_many_arguments)]
async fn scan_input_blob(
    state: &ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    auth: &AuthenticatedKey,
    target: &JobTarget,
    blob: &[u8],
    monitor_hits: &mut Vec<aisix_core::GuardrailMonitorHit>,
    // Accumulated rather than handed back as one handle: the input and
    // output scans resolve their own chain, so a request can produce two
    // audit logs (AISIX-Cloud#1330 / #1024).
    enforced_hits: &mut Vec<aisix_core::GuardrailEnforcedHit>,
    scores: &mut Vec<aisix_core::GuardrailScore>,
    // The request's fail-open bypass tag, accumulated the same way and
    // for the same reason. First one sticks, matching the chain folds.
    bypass: &mut String,
    // The applied set. Both scans resolve the same context, so the first
    // non-empty chain's set is the request's.
    applied: &mut Vec<aisix_core::AppliedGuardrail>,
    // `Some(..)` 表示这个 blob 是最终用户内容：`mask` 规则可以改写它，改写
    // 的计数就记进来。传计数槽本身就是开启遮蔽的开关 —— 于是「遮蔽了却没
    // 有审计痕迹」这种状态压根写不出来。
    //
    // `None` 是控制体（ids、endpoint、超参）：只查不改。在控制体上做遮蔽
    // 会改坏它本该保护的上游引用 —— `input_file_id` 在 JSON 里也不过是个
    // 字符串值 —— 而且这也和 chat 一致：遮蔽改的是消息内容，从不动请求的
    // 控制字段。
    redactions: Option<&mut crate::redact::RedactionCounts>,
) -> Result<Option<Vec<u8>>, ProxyError> {
    let ctx = aisix_guardrails::RequestContext {
        passthrough_route_id: "",
        model_id: &target.model_entry.id,
        mcp_server_id: "",
        api_key_id: &auth.entry.id,
        team_id: auth.key().team_id.as_deref(),
    };
    let chain = state.guardrail_index.resolve(&ctx);
    if chain.is_empty() {
        return Ok(None);
    }
    if applied.is_empty() {
        *applied = chain.applied().to_vec();
    }
    let chat = aisix_gateway::ChatFormat::new(
        target.display_name(),
        vec![aisix_gateway::ChatMessage::user(inspection_text(blob))],
    );
    let (verdict, hits) =
        aisix_guardrails::Guardrail::check_input_unmaskable_observed(&chain, &chat).await;
    monitor_hits.extend(hits);
    let failed_open = verdict.is_bypass();
    // Drained BEFORE the block branch: a `blocked` hit is exactly the one
    // that leaves through `Err`, and the caller's `?` would drop it.
    enforced_hits.extend(chain.enforced_hits());
    scores.extend(chain.scores());
    if bypass.is_empty() {
        if let Some(reason) = chain.bypass_reason() {
            *bypass = reason;
        }
    }
    if let aisix_guardrails::GuardrailVerdict::Block {
        reason,
        guardrail_name,
        unavailable,
    } = verdict
    {
        tracing::warn!(
            guardrail_name = crate::guardrail_stream::guardrail_name_field(guardrail_name.as_deref()),
            guardrail_hook = "input",
            model = %target.display_name(),
            reason = %reason,
            "guardrail blocked jobs request",
        );
        // A control body is what its failure rows report, masked. An uploaded
        // file never is: those rows carry the form's fields and the file's hash,
        // none of which is what the scan read, so they report no body.
        if redactions.is_none() {
            crate::usage_attr::note_refused_request(snapshot, &chain, true, || {
                String::from_utf8_lossy(blob).into_owned()
            });
        }
        return Err(crate::error::guardrail_block_error(
            "request",
            guardrail_name.as_deref(),
            unavailable.as_deref(),
        ));
    }

    // 审查放行了：这条路线之前记下的请求体（控制体，或上传表单的字段和文件
    // 哈希）就是失败行该报告的内容。失败放行时这段文本没被处理过，什么都不报。
    crate::usage_attr::release_failure_request();
    if failed_open {
        crate::usage_attr::note_failure_request(snapshot, String::new);
    }

    // #932 的顺序：先过阻断，再改写，改写后的字节才是上游收到的东西。
    let Some(redactions) = redactions else {
        return Ok(None);
    };
    if !aisix_guardrails::Guardrail::redacts_input(&chain) {
        return Ok(None);
    }
    // 改写是在文本 span 上做替换，所以 blob 得能按 UTF-8 读回来。把 lossy
    // 转换的结果写回去会毁掉一个非文本上传 —— 那种 blob 只查不改。
    let Ok(text) = std::str::from_utf8(blob) else {
        // 可也不能因此不经遮蔽就转发：一个 0xFF 字节就能让一整份 PII 原样过
        // 去。遮蔽规则在审查读的文本里有任何命中，就拒绝上传。
        if aisix_guardrails::Guardrail::redact_input_text(&chain, &inspection_text(blob)).is_some()
        {
            return Err(crate::error::guardrail_block_error("request", None, None));
        }
        return Ok(None);
    };
    // 严格解析不了、却带转义的行，字符串改写不回去：遮蔽规则命中它们解码后的
    // 内容就拒绝上传。
    let unparsed = unparsed_escaped_literals(text);
    if !unparsed.is_empty()
        && aisix_guardrails::Guardrail::redact_input_text(&chain, &unparsed).is_some()
    {
        return Err(crate::error::guardrail_block_error("request", None, None));
    }
    // batch / 微调文件是 JSONL：逐行按请求内容改写，协议字段不动；协议字段
    // 本身或对象的键命中 mask 规则时拒绝上传（与其它端点对结构字段的处理一致）。
    // 有的行是 JSON 对象、有的行不是的文件也拒绝：两种改写方式都不对。
    if let Some(r) = crate::redact::redact_jsonl_upload(&chain, text) {
        if r.unrewritable {
            return Err(crate::error::guardrail_block_error("request", None, None));
        }
        // 转发的是逐行重新序列化、检查过的那份，改没改都一样（见
        // `redact_jsonl_upload`）。
        crate::redact::merge_counts(redactions, r.counts);
        return Ok(Some(r.text.into_bytes()));
    }
    // 没有一行是 JSON 对象的文件是纯文本，没有协议字段，整段改写。
    let Some(r) = aisix_guardrails::Guardrail::redact_input_text(&chain, text) else {
        return Ok(None);
    };
    crate::redact::merge_counts(redactions, r.counts);
    Ok(Some(r.text.into_bytes()))
}

/// Run the resolved output-guardrail chain over an upstream body, as one
/// synthetic assistant turn. The body is decoded best-effort and relayed
/// as the provider sent it.
#[allow(clippy::too_many_arguments)]
async fn scan_output_blob(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    target: &JobTarget,
    blob: &[u8],
    monitor_hits: &mut Vec<aisix_core::GuardrailMonitorHit>,
    // Accumulated rather than handed back as one handle: the input and
    // output scans resolve their own chain, so a request can produce two
    // audit logs (AISIX-Cloud#1330 / #1024).
    enforced_hits: &mut Vec<aisix_core::GuardrailEnforcedHit>,
    scores: &mut Vec<aisix_core::GuardrailScore>,
    // The request's fail-open bypass tag, accumulated the same way and
    // for the same reason. First one sticks, matching the chain folds.
    bypass: &mut String,
    // The applied set. Both scans resolve the same context, so the first
    // non-empty chain's set is the request's.
    applied: &mut Vec<aisix_core::AppliedGuardrail>,
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
    if applied.is_empty() {
        *applied = chain.applied().to_vec();
    }
    let synth = aisix_gateway::ChatResponse {
        logprobs: None,
        more_choices: Vec::new(),
        id: String::new(),
        model: target.display_name().to_string(),
        message: aisix_gateway::ChatMessage::assistant(String::from_utf8_lossy(blob).into_owned()),
        finish_reason: aisix_gateway::FinishReason::Stop,
        usage: aisix_gateway::UsageStats::default(),
        service_tier: None,
    };
    let (verdict, hits) =
        aisix_guardrails::Guardrail::check_output_unmaskable_observed(&chain, &synth).await;
    monitor_hits.extend(hits);
    // See `scan_input_blob`.
    enforced_hits.extend(chain.enforced_hits());
    scores.extend(chain.scores());
    if bypass.is_empty() {
        if let Some(reason) = chain.bypass_reason() {
            *bypass = reason;
        }
    }
    if let aisix_guardrails::GuardrailVerdict::Block {
        reason,
        guardrail_name,
        unavailable,
    } = verdict
    {
        tracing::warn!(
            guardrail_name = crate::guardrail_stream::guardrail_name_field(guardrail_name.as_deref()),
            guardrail_hook = "output",
            model = %target.display_name(),
            reason = %reason,
            "guardrail blocked jobs response",
        );
        return Err(crate::error::guardrail_block_error(
            "response",
            guardrail_name.as_deref(),
            unavailable.as_deref(),
        ));
    }
    Ok(())
}

// ─────────────────────── response id rewrite ───────────────────────

/// Top-level id fields the gateway re-encodes on create/retrieve
/// responses so later calls route automatically. List responses are
/// forwarded untouched (raw ids there fall back to
/// explicit/default routing on the next call).
const ID_FIELDS: &[&str] = &[
    "id",
    "input_file_id",
    "output_file_id",
    "error_file_id",
    "training_file",
    "validation_file",
];

/// 网关自己的标记不是调用方写的 metadata，不该出现在给它的应答里。
fn strip_gateway_markers(v: &mut Value) {
    if let Some(Value::Object(metadata)) = v.get_mut("metadata") {
        metadata.remove(BATCH_SUBMITTER_KEY);
        metadata.remove(BATCH_MODEL_KEY);
    }
}

fn rewrite_response_ids(v: &mut Value, model: &str, owner: Option<(&str, &str)>) {
    strip_gateway_markers(v);
    let Some(obj) = v.as_object_mut() else { return };
    for field in ID_FIELDS {
        if let Some(Value::String(s)) = obj.get_mut(*field) {
            if !s.is_empty() && !s.starts_with(ROUTED_ID_PREFIX) {
                *s = encode_routed_id(s, model, owner);
            }
        }
    }
    // Fine-tuning: `result_files` is an array of file ids.
    if let Some(Value::Array(files)) = obj.get_mut("result_files") {
        for f in files {
            if let Value::String(s) = f {
                if !s.is_empty() && !s.starts_with(ROUTED_ID_PREFIX) {
                    *s = encode_routed_id(s, model, owner);
                }
            }
        }
    }
}

// ─────────────────────── telemetry plumbing ───────────────────────

/// A multipart field joins the capture beside any earlier one of its name:
/// every repeat is forwarded, so none may disappear from the export.
fn push_captured(captured: &mut serde_json::Map<String, Value>, key: String, value: String) {
    match captured.get_mut(&key) {
        Some(Value::String(existing)) => {
            existing.push('\n');
            existing.push_str(&value);
        }
        _ => {
            captured.insert(key, Value::String(value));
        }
    }
}

/// Zero-token UsageEvent for one management call (create/list/retrieve/
/// cancel/delete). Same contract as the `/passthrough` event (#699),
/// but with the resolved Model attributed.
#[allow(clippy::too_many_arguments)]
fn emit_job_usage_event(
    state: &ProxyState,
    // The request's snapshot, loaded once by the handler (#941).
    snap: &aisix_core::AisixSnapshot,
    surface: crate::operation::Surface,
    request_id: &str,
    auth: &AuthenticatedKey,
    target: &JobTarget,
    status_code: u16,
    elapsed: Duration,
    client: &ClientContext,
    guardrail_monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
    // 遮蔽只记类型和条数，不记被遮蔽的内容 —— 事件本身不能变成 PII 汇集点。
    redacted_entity_counts: crate::redact::RedactionCounts,
    applied_guardrails: Vec<aisix_core::AppliedGuardrail>,
    // What an ENFORCING guardrail actually did to this request
    // (AISIX-Cloud#1330).
    guardrail_enforced_hits: Vec<aisix_core::GuardrailEnforcedHit>,
    guardrail_scores: Vec<aisix_core::GuardrailScore>,
    guardrail_bypassed_reason: String,
) {
    // A refused job call is answered with the upstream's own response, so no
    // error reaches this point.
    let (error_class, error_message) = if crate::attempt::relayed_response_failed(status_code) {
        let (class, message) = crate::attempt::relayed_failure(
            status_code,
            &crate::attribution::relayed_error_body().unwrap_or_default(),
        );
        (class.to_string(), message)
    } else {
        Default::default()
    };
    let mut event = UsageEvent {
        request_id: request_id.to_string(),
        occurred_at: aisix_obs::UsageEvent::occurred_at_now(),
        model_id: target.model_entry.id.clone(),
        api_key_id: auth.entry.id.clone(),
        requested_model: target.display_name().to_string(),
        // The jobs surface only routes to a direct model, which is the entry
        // `requested_model` names here, including the auto-selected one.
        requested_model_id: target.model_entry.id.clone(),
        status_code,
        // Single-attempt endpoint: the attempt spans the whole request, so
        // the upstream figure and what the caller waited for coincide.
        upstream_latency_ms: crate::attempt::measured_ms(elapsed),
        downstream_latency_ms: crate::attempt::measured_ms(elapsed),
        inbound_protocol: "openai".to_string(),
        client_source_ip: client.source_ip.clone(),
        client_user_agent: client.user_agent.clone(),
        request_headers: client.request_headers.clone(),
        guardrail_monitor_hits,
        redacted_entity_counts,
        applied_guardrails,
        guardrail_enforced_hits,
        guardrail_scores,
        guardrail_bypassed_reason,
        error_class,
        error_message,
        ..Default::default()
    };
    let pk = crate::usage_attr::ResolvedPk::resolve(snap, &target.pk_entry.id);
    crate::usage_attr::apply_pk_telemetry(&mut event, &pk);
    crate::usage_attr::apply_caller_identity(&mut event, auth.jwt.as_ref(), &auth.caller());
    let usage_model =
        crate::usage_attr::usage_event_model_label(snap, &event.requested_model).into_owned();
    crate::usage_attr::emit_usage(
        state,
        snap,
        surface,
        event,
        crate::usage_attr::usage_event_labels(&usage_model, &pk),
        None,
        client.trace.as_ref(),
        /* terminal */ true,
        /* dispatched */ true,
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
    let log_target = crate::attribution::AccessLogTarget::current();
    crate::attribution::emit_access_log(AccessLog {
        method: method.as_str(),
        path,
        status,
        latency: elapsed,
        duration: elapsed,
        provider: target.map(|t| t.provider_label()).filter(|p| !p.is_empty()),
        model: target.map(|t| t.display_name()),
        upstream_model: log_target.upstream_model(),
        provider_key_id: log_target.provider_key_id(),
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
        mcp: None,
        cache: None,
        request_body_bytes: None,
        response_body_bytes: None,
    });
}

/// Shared success/error tail for every handler: access log, metrics,
/// usage event, response header.
#[allow(clippy::too_many_arguments)]
async fn finish(
    state: &ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    surface: crate::operation::Surface,
    method: Method,
    path: String,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    started: Instant,
    request_id: String,
    result: Result<(Response, JobTarget), ProxyError>,
    monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
    redactions: crate::redact::RedactionCounts,
    enforced_hits: Vec<aisix_core::GuardrailEnforcedHit>,
    scores: Vec<aisix_core::GuardrailScore>,
    bypass: String,
    applied: Vec<aisix_core::AppliedGuardrail>,
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
                crate::attempt::relayed_error_source(status),
                elapsed,
            );
            emit_job_usage_event(
                state,
                snapshot,
                surface,
                &request_id,
                auth,
                &target,
                status,
                elapsed,
                client,
                monitor_hits,
                redactions,
                applied,
                enforced_hits,
                scores,
                bypass,
            );
            // 这条路和 /v1/chat/completions 花的是同一个 key 级配额桶
            // （`quota::enforce` 预留的就是请求级那几层），所以也得照实
            // 汇报：不发头，SDK 只能盲目重试撞 429；不发 gauge，
            // `aisix_rate_limit_remaining` 会一直停在上一次 chat 请求的读数，
            // 而这个 key 其实已经逼近上限了。
            let rl_limits = auth.key().rate_limit.clone().unwrap_or_default();
            crate::request_metrics::publish_rate_limit_window(
                &state.metrics,
                &state.limiter,
                snapshot,
                &auth.entry.id,
                &rl_limits,
                target.display_name(),
                target
                    .model_entry
                    .value
                    .upstream_model()
                    .unwrap_or_default(),
                &mut resp,
            )
            .await;
            if let Ok(hv) = HeaderValue::from_str(&request_id) {
                resp.headers_mut().insert("x-aisix-request-id", hv);
            }
            resp
        }
        Err(err) => {
            let status = err.status().as_u16();
            crate::usage_attr::note_client_error(snapshot, &err);
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
                    model: surface.handler,
                    ..Default::default()
                },
                status,
                crate::attempt::error_source(&err),
                elapsed,
            );
            crate::usage_attr::emit_error_usage_event(
                state,
                snapshot,
                surface,
                "openai",
                &request_id,
                "",
                &auth.entry.id,
                status,
                crate::usage_attr::EventError::of(&err),
                err.is_guardrail_block(),
                client,
                applied,
                enforced_hits,
                scores,
                bypass,
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
    owner: Option<(&str, &str)>,
) -> Response {
    let body = match (rewrite_model, serde_json::from_slice::<Value>(&bytes).ok()) {
        (Some(model), Some(mut v)) => {
            rewrite_response_ids(&mut v, model, owner);
            Bytes::from(serde_json::to_vec(&v).unwrap_or_else(|_| bytes.to_vec()))
        }
        // A list keeps its ids, but not the gateway's own markers on each
        // batch: attribution read them off the upstream's bytes already.
        (None, Some(mut v))
            if bytes
                .windows(BATCH_SUBMITTER_KEY.len())
                .any(|w| w == BATCH_SUBMITTER_KEY.as_bytes()) =>
        {
            if let Some(Value::Array(items)) = v.get_mut("data") {
                items.iter_mut().for_each(strip_gateway_markers);
            }
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
/// id-embedded → explicit query/header → default. The decoded gateway id
/// beside it, for [`require_job_owner`].
fn routed_model_hint(
    id: &str,
    params: &HashMap<String, String>,
    headers: &HeaderMap,
) -> (String, Option<String>, Option<RoutedId>) {
    match decode_routed_id(id) {
        Some(routed) => (routed.raw.clone(), Some(routed.model.clone()), Some(routed)),
        None => (id.to_string(), explicit_model(params, headers), None),
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
    let mut redactions = crate::redact::RedactionCounts::new();
    let mut enforced_hits: Vec<aisix_core::GuardrailEnforcedHit> = Vec::new();
    let mut scores: Vec<aisix_core::GuardrailScore> = Vec::new();
    let mut bypass = String::new();
    let mut applied: Vec<aisix_core::AppliedGuardrail> = Vec::new();
    // Loaded below, after the upload is drained — see the note in
    // `audio::multipart_dispatch` (#941 audit M2).
    let mut snapshot = None;

    let result = async {
        // Re-build the outbound multipart form, extracting the gateway-only
        // `model` routing field. Text fields (purpose, expires_after[...])
        // forward verbatim.
        // 表单要等文件体过完护栏（可能被遮蔽）之后才能拼，所以先按收到的
        // 顺序原样留存 —— multipart 的部件顺序对上游可见，不该因为这次改动
        // 变了样。
        let mut fields: Vec<UploadField> = Vec::new();
        let mut form_model: Option<String> = None;
        // The upload as a failure row reports it: the text fields, with the
        // file represented by its checksum (the audio convention).
        let capture = crate::usage_attr::content_cap(&state.snapshot.load()).is_some();
        let mut captured = serde_json::Map::new();

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
                if capture {
                    use sha2::Digest;
                    push_captured(
                        &mut captured,
                        "file_sha256".to_string(),
                        format!("{:x}", sha2::Sha256::digest(&bytes)),
                    );
                }
                fields.push(UploadField::File {
                    file_name,
                    content_type,
                    bytes,
                });
                continue;
            }
            let v = field.text().await.map_err(|e| {
                crate::error::proxy_error_from_multipart(
                    e,
                    state.request_body_limit_for("/v1/files"),
                    "malformed multipart field",
                )
            })?;
            if capture {
                push_captured(&mut captured, name.clone(), v.clone());
            }
            fields.push(UploadField::Text(name, v));
        }

        let snapshot = &**snapshot.insert(state.snapshot.load());
        crate::usage_attr::note_failure_request(snapshot, || Value::Object(captured).to_string());
        let wanted = form_model.or_else(|| explicit_model(&params, &headers));
        crate::usage_attr::hold_ahead_of_chain(&state, snapshot, &auth, wanted.as_deref());

        if !fields.iter().any(|f| matches!(f, UploadField::File { .. })) {
            return Err(ProxyError::InvalidRequest(
                "multipart body must include a `file` field".into(),
            ));
        }

        let target = resolve_target(snapshot, &auth, wanted.as_deref(), &client)?;

        // Batch/fine-tune input files carry end-user content — scan them
        // like any other inbound payload. 这也是这条面上唯一承载最终用户
        // 提示词的载荷，所以是唯一允许被 `mask` 规则改写的那一个。
        // Every `file` part is forwarded, so every one is scanned (and may
        // be masked). Keyed by position, so each rewrite lands on the part it
        // was made from.
        let early_reservation =
            reserve_before_costly_scan(&state, snapshot, &auth, &target).await?;
        let mut masked: std::collections::HashMap<usize, Vec<u8>> =
            std::collections::HashMap::new();
        let mut file_parts = 0usize;
        for (i, field) in fields.iter().enumerate() {
            if let UploadField::File { bytes, .. } = field {
                // 审查调远端服务时，第一个以外的每个 file 部件也各算一次请求，
                // 在审它之前就占：否则一个表单塞上几十个部件，只占一格限额就换
                // 来几十倍的远端调用。这一格马上放掉 —— 要的只是计数，并发名额
                // 由上面那一格占着。
                if early_reservation.is_some() && file_parts > 0 {
                    drop(reserve_target(&state, snapshot, &auth, &target).await?);
                }
                file_parts += 1;
                if let Some(m) = scan_input_blob(
                    &state,
                    snapshot,
                    &auth,
                    &target,
                    bytes,
                    &mut monitor_hits,
                    &mut enforced_hits,
                    &mut scores,
                    &mut bypass,
                    &mut applied,
                    Some(&mut redactions),
                )
                .await?
                {
                    masked.insert(i, m);
                }
            }
        }

        let mut form = reqwest::multipart::Form::new();
        for (i, field) in fields.into_iter().enumerate() {
            match field {
                UploadField::Text(name, v) => form = form.text(name, v),
                UploadField::File {
                    file_name,
                    content_type,
                    bytes,
                } => {
                    // Over the `Bytes` itself, not a `Vec` copy of the file.
                    let body = masked.remove(&i).map_or(bytes, Bytes::from);
                    let len = body.len() as u64;
                    let mut part = reqwest::multipart::Part::stream_with_length(body, len)
                        .file_name(file_name);
                    if let Some(ct) = content_type {
                        part = part.mime_str(&ct).map_err(|e| {
                            ProxyError::InvalidRequest(format!("invalid file content-type: {e}"))
                        })?;
                    }
                    form = form.part("file", part);
                }
            }
        }
        let _reservation = match early_reservation {
            Some(reservation) => reservation,
            None => reserve_target(&state, snapshot, &auth, &target).await?,
        };
        crate::budget::gate(&state, snapshot).check_provider_key(&target.pk_entry.id)?;

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
        let model = target.display_name().to_string();
        Ok((
            json_response(
                status,
                &resp_headers,
                bytes,
                Some(&model),
                owner_seal(&auth, snapshot, &target)
                    .as_ref()
                    .map(|(owner, secret)| (owner.as_str(), *secret)),
            ),
            target,
        ))
    }
    .await;

    finish(
        &state,
        // A form that failed before the model field was read never loaded
        // one.
        &snapshot.unwrap_or_else(|| state.snapshot.load()),
        crate::operation::FILES,
        Method::POST,
        "/v1/files".into(),
        &auth,
        &client,
        started,
        request_id,
        result,
        monitor_hits,
        redactions,
        enforced_hits,
        scores,
        bypass,
        applied,
    )
    .await
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
            surface: crate::operation::FILES,
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
    let (raw, embedded, routed) = routed_model_hint(&id, &params, &headers);
    forward_simple(
        state,
        auth,
        client,
        params,
        headers,
        FwdSpec {
            surface: crate::operation::FILES,
            method: Method::GET,
            log_path: format!("/v1/files/{id}"),
            upstream_path: format!("/files/{raw}"),
            raw_query: None,
            body: None,
            id: Some((raw, embedded, routed)),
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
    let (raw, embedded, routed) = routed_model_hint(&id, &params, &headers);
    forward_simple(
        state,
        auth,
        client,
        params,
        headers,
        FwdSpec {
            surface: crate::operation::FILES,
            method: Method::DELETE,
            log_path: format!("/v1/files/{id}"),
            upstream_path: format!("/files/{raw}"),
            raw_query: None,
            body: None,
            id: Some((raw, embedded, routed)),
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
    let (raw, embedded, routed) = routed_model_hint(&id, &params, &headers);
    forward_simple(
        state,
        auth,
        client,
        params,
        headers,
        FwdSpec {
            surface: crate::operation::FILES,
            method: Method::GET,
            log_path: format!("/v1/files/{id}/content"),
            upstream_path: format!("/files/{raw}/content"),
            raw_query: None,
            body: None,
            id: Some((raw, embedded, routed)),
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
    // 控制体路线只查不改：计数表恒空，仍然交给 `finish`，这样签名只有一份。
    let redactions = crate::redact::RedactionCounts::new();
    let mut enforced_hits: Vec<aisix_core::GuardrailEnforcedHit> = Vec::new();
    let mut scores: Vec<aisix_core::GuardrailScore> = Vec::new();
    let mut bypass = String::new();
    let mut applied: Vec<aisix_core::AppliedGuardrail> = Vec::new();

    // One snapshot for the whole request (#941) — see `embeddings`.
    let snapshot = state.snapshot.load();
    crate::usage_attr::note_failure_request(&snapshot, || {
        String::from_utf8_lossy(&body).into_owned()
    });
    // 模型名可能还在请求体里，解析之后再按它补一次（见下）。
    crate::usage_attr::hold_ahead_of_chain(
        &state,
        &snapshot,
        &auth,
        explicit_model(&params, &headers).as_deref(),
    );

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

        // Routing precedence (a mainstream gateway's scenario order): file-id-embedded
        // model → explicit body `model` / query / header → default.
        let routed = decode_routed_id(&input_file_id);
        let (raw_file_id, embedded) = match &routed {
            Some(id) => (id.raw.clone(), Some(id.model.clone())),
            None => (input_file_id.clone(), None),
        };
        require_safe_upstream_id(&raw_file_id)?;
        let body_model = req_json
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
        let wanted = embedded
            .or(body_model)
            .or_else(|| explicit_model(&params, &headers));
        crate::usage_attr::hold_ahead_of_chain(&state, &snapshot, &auth, wanted.as_deref());
        let target = resolve_target(&snapshot, &auth, wanted.as_deref(), &client)?;
        // A job built on a file is a read of that file.
        require_job_owner(&auth, &snapshot, &target, &raw_file_id, routed.as_ref())?;

        // Forward the provider wire shape: raw file id, no gateway-only
        // routing fields.
        if let Some(obj) = req_json.as_object_mut() {
            obj.insert("input_file_id".into(), Value::String(raw_file_id));
            obj.remove("model");
            obj.remove("provider");
        }
        let out_body = Bytes::from(serde_json::to_vec(&req_json).unwrap_or_default());

        let early_reservation =
            reserve_before_costly_scan(&state, &snapshot, &auth, &target).await?;
        scan_input_blob(
            &state,
            &snapshot,
            &auth,
            &target,
            &out_body,
            &mut monitor_hits,
            &mut enforced_hits,
            &mut scores,
            &mut bypass,
            &mut applied,
            None,
        )
        .await?;
        let _reservation = match early_reservation {
            Some(reservation) => reservation,
            None => reserve_target(&state, &snapshot, &auth, &target).await?,
        };
        crate::budget::gate(&state, &snapshot).check_provider_key(&target.pk_entry.id)?;

        // 放在审查之后：审查看的是调用方写的内容，不是网关自己的标记。
        stamp_batch_submitter(&mut req_json, &auth.entry.id, target.display_name());
        let out_body = Bytes::from(serde_json::to_vec(&req_json).unwrap_or_default());

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
        // 放在输出审查之前：审查拦下应答，批次在上游也已经建好了，照样要花钱。
        if status.is_success() {
            let created = serde_json::from_slice::<Value>(&bytes).ok();
            if let Some(raw_id) = created.as_ref().and_then(|v| v["id"].as_str()) {
                watch_batch(&state, &auth, &client, &target, raw_id);
            }
        }
        scan_output_blob(
            &state,
            &auth,
            &target,
            &bytes,
            &mut monitor_hits,
            &mut enforced_hits,
            &mut scores,
            &mut bypass,
            &mut applied,
        )
        .await?;
        let model = target.display_name().to_string();
        Ok((
            json_response(
                status,
                &resp_headers,
                bytes,
                Some(&model),
                owner_seal(&auth, &snapshot, &target)
                    .as_ref()
                    .map(|(owner, secret)| (owner.as_str(), *secret)),
            ),
            target,
        ))
    }
    .await;

    finish(
        &state,
        &snapshot,
        crate::operation::BATCHES,
        Method::POST,
        "/v1/batches".into(),
        &auth,
        &client,
        started,
        request_id,
        result,
        monitor_hits,
        redactions,
        enforced_hits,
        scores,
        bypass,
        applied,
    )
    .await
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
    let (raw, embedded, routed) = routed_model_hint(&id, &params, &headers);
    let mut monitor_hits: Vec<aisix_core::GuardrailMonitorHit> = Vec::new();
    // 控制体路线只查不改：计数表恒空，仍然交给 `finish`，这样签名只有一份。
    let redactions = crate::redact::RedactionCounts::new();
    let mut enforced_hits: Vec<aisix_core::GuardrailEnforcedHit> = Vec::new();
    let mut scores: Vec<aisix_core::GuardrailScore> = Vec::new();
    let mut bypass = String::new();
    let mut applied: Vec<aisix_core::AppliedGuardrail> = Vec::new();

    // One snapshot for the whole request (#941) — see `embeddings`.
    let snapshot = state.snapshot.load();

    let result = async {
        require_safe_upstream_id(&raw)?;
        let wanted = embedded.or_else(|| explicit_model(&params, &headers));
        let target = resolve_target(&snapshot, &auth, wanted.as_deref(), &client)?;
        require_job_owner(&auth, &snapshot, &target, &raw, routed.as_ref())?;
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
        crate::budget::gate(&state, &snapshot).check_provider_key(&target.pk_entry.id)?;

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
        scan_output_blob(
            &state,
            &auth,
            &target,
            &bytes,
            &mut monitor_hits,
            &mut enforced_hits,
            &mut scores,
            &mut bypass,
            &mut applied,
        )
        .await?;

        // Batch cost attribution (#720): first observation of a completed
        // batch downloads the output JSONL and emits real token usage.
        if status == StatusCode::OK {
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                maybe_attribute_batch(&state, &auth, &client, &target, &raw, &v);
            }
        }

        let model = target.display_name().to_string();
        Ok((
            json_response(
                status,
                &resp_headers,
                bytes,
                Some(&model),
                owner_seal(&auth, &snapshot, &target)
                    .as_ref()
                    .map(|(owner, secret)| (owner.as_str(), *secret)),
            ),
            target,
        ))
    }
    .await;

    finish(
        &state,
        &snapshot,
        crate::operation::BATCHES,
        Method::GET,
        format!("/v1/batches/{id}"),
        &auth,
        &client,
        started,
        request_id,
        result,
        monitor_hits,
        redactions,
        enforced_hits,
        scores,
        bypass,
        applied,
    )
    .await
}

pub(crate) async fn cancel_batch(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (raw, embedded, routed) = routed_model_hint(&id, &params, &headers);
    forward_simple(
        state,
        auth,
        client,
        params,
        headers,
        FwdSpec {
            surface: crate::operation::BATCHES,
            method: Method::POST,
            log_path: format!("/v1/batches/{id}/cancel"),
            upstream_path: format!("/batches/{raw}/cancel"),
            raw_query: None,
            body: None,
            id: Some((raw, embedded, routed)),
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
            surface: crate::operation::BATCHES,
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
    // 控制体路线只查不改：计数表恒空，仍然交给 `finish`，这样签名只有一份。
    let redactions = crate::redact::RedactionCounts::new();
    let mut enforced_hits: Vec<aisix_core::GuardrailEnforcedHit> = Vec::new();
    let mut scores: Vec<aisix_core::GuardrailScore> = Vec::new();
    let mut bypass = String::new();
    let mut applied: Vec<aisix_core::AppliedGuardrail> = Vec::new();

    // One snapshot for the whole request (#941) — see `embeddings`.
    let snapshot = state.snapshot.load();
    crate::usage_attr::note_failure_request(&snapshot, || {
        String::from_utf8_lossy(&body).into_owned()
    });
    // 模型名可能还在请求体里，解析之后再按它补一次（见下）。
    crate::usage_attr::hold_ahead_of_chain(
        &state,
        &snapshot,
        &auth,
        explicit_model(&params, &headers).as_deref(),
    );

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
        let routed = decode_routed_id(&training_file);
        let (raw_training, embedded) = match &routed {
            Some(id) => (id.raw.clone(), Some(id.model.clone())),
            None => (training_file.clone(), None),
        };
        require_safe_upstream_id(&raw_training)?;
        let wanted = embedded.or_else(|| explicit_model(&params, &headers));
        crate::usage_attr::hold_ahead_of_chain(&state, &snapshot, &auth, wanted.as_deref());
        let target = resolve_target(&snapshot, &auth, wanted.as_deref(), &client)?;
        // See `create_batch`.
        require_job_owner(&auth, &snapshot, &target, &raw_training, routed.as_ref())?;

        if let Some(obj) = req_json.as_object_mut() {
            obj.insert("training_file".into(), Value::String(raw_training));
            if let Some(vf) = obj.get("validation_file").and_then(Value::as_str) {
                match decode_routed_id(vf) {
                    Some(id) => {
                        require_safe_upstream_id(&id.raw)?;
                        require_job_owner(&auth, &snapshot, &target, &id.raw, Some(&id))?;
                        obj.insert("validation_file".into(), Value::String(id.raw));
                    }
                    None => require_job_owner(&auth, &snapshot, &target, vf, None)?,
                }
            }
            obj.remove("provider");
        }
        let out_body = Bytes::from(serde_json::to_vec(&req_json).unwrap_or_default());

        let early_reservation =
            reserve_before_costly_scan(&state, &snapshot, &auth, &target).await?;
        scan_input_blob(
            &state,
            &snapshot,
            &auth,
            &target,
            &out_body,
            &mut monitor_hits,
            &mut enforced_hits,
            &mut scores,
            &mut bypass,
            &mut applied,
            None,
        )
        .await?;
        let _reservation = match early_reservation {
            Some(reservation) => reservation,
            None => reserve_target(&state, &snapshot, &auth, &target).await?,
        };
        crate::budget::gate(&state, &snapshot).check_provider_key(&target.pk_entry.id)?;

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
        scan_output_blob(
            &state,
            &auth,
            &target,
            &bytes,
            &mut monitor_hits,
            &mut enforced_hits,
            &mut scores,
            &mut bypass,
            &mut applied,
        )
        .await?;
        let model = target.display_name().to_string();
        Ok((
            json_response(
                status,
                &resp_headers,
                bytes,
                Some(&model),
                owner_seal(&auth, &snapshot, &target)
                    .as_ref()
                    .map(|(owner, secret)| (owner.as_str(), *secret)),
            ),
            target,
        ))
    }
    .await;

    finish(
        &state,
        &snapshot,
        crate::operation::FINE_TUNING,
        Method::POST,
        "/v1/fine_tuning/jobs".into(),
        &auth,
        &client,
        started,
        request_id,
        result,
        monitor_hits,
        redactions,
        enforced_hits,
        scores,
        bypass,
        applied,
    )
    .await
}

pub(crate) async fn get_ft_job(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (raw, embedded, routed) = routed_model_hint(&id, &params, &headers);
    forward_simple(
        state,
        auth,
        client,
        params,
        headers,
        FwdSpec {
            surface: crate::operation::FINE_TUNING,
            method: Method::GET,
            log_path: format!("/v1/fine_tuning/jobs/{id}"),
            upstream_path: format!("/fine_tuning/jobs/{raw}"),
            raw_query: None,
            body: None,
            id: Some((raw, embedded, routed)),
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
    let (raw, embedded, routed) = routed_model_hint(&id, &params, &headers);
    forward_simple(
        state,
        auth,
        client,
        params,
        headers,
        FwdSpec {
            surface: crate::operation::FINE_TUNING,
            method: Method::POST,
            log_path: format!("/v1/fine_tuning/jobs/{id}/cancel"),
            upstream_path: format!("/fine_tuning/jobs/{raw}/cancel"),
            raw_query: None,
            body: None,
            id: Some((raw, embedded, routed)),
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
            surface: crate::operation::FINE_TUNING,
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
    surface: crate::operation::Surface,
    method: Method,
    log_path: String,
    upstream_path: String,
    raw_query: Option<String>,
    body: Option<Bytes>,
    /// `(raw_id, id_embedded_model, id_owner)` for id-addressed calls; the raw
    /// id is charset-validated before touching the upstream URL. `None` is a
    /// list.
    id: Option<(String, Option<String>, Option<RoutedId>)>,
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
    let surface = spec.surface;
    let mut monitor_hits: Vec<aisix_core::GuardrailMonitorHit> = Vec::new();
    // 控制体路线只查不改：计数表恒空，仍然交给 `finish`，这样签名只有一份。
    let redactions = crate::redact::RedactionCounts::new();
    let mut enforced_hits: Vec<aisix_core::GuardrailEnforcedHit> = Vec::new();
    let mut scores: Vec<aisix_core::GuardrailScore> = Vec::new();
    let mut bypass = String::new();
    let mut applied: Vec<aisix_core::AppliedGuardrail> = Vec::new();

    // One snapshot for the whole request (#941) — see `embeddings`.
    let snapshot = state.snapshot.load();
    if let Some(body) = &spec.body {
        crate::usage_attr::note_failure_request(&snapshot, || {
            String::from_utf8_lossy(body).into_owned()
        });
        let model = spec
            .id
            .as_ref()
            .and_then(|(_, embedded, _)| embedded.clone())
            .or_else(|| explicit_model(&params, &headers));
        crate::usage_attr::hold_ahead_of_chain(&state, &snapshot, &auth, model.as_deref());
    }

    let result = async {
        let embedded = match &spec.id {
            Some((raw, embedded, _)) => {
                require_safe_upstream_id(raw)?;
                embedded.clone()
            }
            None => None,
        };
        let wanted = embedded.or_else(|| explicit_model(&params, &headers));
        let target = resolve_target(&snapshot, &auth, wanted.as_deref(), &client)?;
        match &spec.id {
            Some((raw, _, routed)) => {
                require_job_owner(&auth, &snapshot, &target, raw, routed.as_ref())?
            }
            None => require_job_list(&auth, &snapshot, &target)?,
        }

        let early_reservation =
            reserve_before_costly_scan(&state, &snapshot, &auth, &target).await?;
        if let Some(body) = &spec.body {
            scan_input_blob(
                &state,
                &snapshot,
                &auth,
                &target,
                body,
                &mut monitor_hits,
                &mut enforced_hits,
                &mut scores,
                &mut bypass,
                &mut applied,
                None,
            )
            .await?;
        }
        let _reservation = match early_reservation {
            Some(reservation) => reservation,
            None => reserve_target(&state, &snapshot, &auth, &target).await?,
        };
        crate::budget::gate(&state, &snapshot).check_provider_key(&target.pk_entry.id)?;

        let query = forwardable_query(spec.raw_query.as_deref());
        let url = upstream_url(&target, &spec.upstream_path, &query)?;
        let body = match spec.body {
            Some(b) => UpstreamBody::Json(b),
            None => UpstreamBody::Empty,
        };
        let (status, resp_headers, bytes) =
            send_upstream(&state, &target, spec.method, &url, body, &request_id).await?;
        if surface == crate::operation::BATCHES && method == Method::GET && status.is_success() {
            attribute_listed_batches(&state, &auth, &client, &target, &bytes);
        }
        // The files surface answers with opaque payloads, not model output:
        // a download is whatever the provider stored under an id. Screening
        // it as one synthetic message is #1120's job, per record, through
        // the ordinary response chain. An upload IS screened, line by line,
        // in `create_file`.
        if surface != crate::operation::FILES {
            scan_output_blob(
                &state,
                &auth,
                &target,
                &bytes,
                &mut monitor_hits,
                &mut enforced_hits,
                &mut scores,
                &mut bypass,
                &mut applied,
            )
            .await?;
        }

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
            json_response(
                status,
                &resp_headers,
                bytes,
                model.as_deref(),
                owner_seal(&auth, &snapshot, &target)
                    .as_ref()
                    .map(|(owner, secret)| (owner.as_str(), *secret)),
            )
        };
        Ok((resp, target))
    }
    .await;

    finish(
        &state,
        &snapshot,
        surface,
        method,
        log_path,
        &auth,
        &client,
        started,
        request_id,
        result,
        monitor_hits,
        redactions,
        enforced_hits,
        scores,
        bypass,
        applied,
    )
    .await
}

// ─────────────────── batch completion attribution ───────────────────

/// The service tier of one aggregated batch row: known only while every
/// line folded into the row reports the same non-empty tier.
#[derive(Default)]
enum TierAgreement {
    #[default]
    NoLines,
    Agreed(String),
    Mixed,
}

impl TierAgreement {
    fn observe(&mut self, line_tier: Option<&str>) {
        let line_tier = line_tier.filter(|t| !t.is_empty());
        *self = match (std::mem::take(self), line_tier) {
            (Self::NoLines, Some(t)) => Self::Agreed(t.to_owned()),
            (Self::Agreed(prev), Some(t)) if prev == t => Self::Agreed(prev),
            _ => Self::Mixed,
        };
    }

    fn agreed(&self) -> Option<&str> {
        match self {
            Self::Agreed(t) => Some(t),
            Self::NoLines | Self::Mixed => None,
        }
    }
}

/// Deterministic UUIDv5 request_id for a batch attribution event.
///
/// The same (batch, model-slice) always gets the same id: a restarted
/// process or another replica re-emits a batch's rows under it, and the
/// control plane drops the re-sent row at ingest by `(env_id, request_id)`
/// while the original is still within usage retention. A UUID rather than a
/// readable id keeps a synthesized batch row distinguishable from a
/// caller-supplied request id.
fn batch_attribution_request_id(raw_batch_id: &str, idx: usize, multi: bool) -> String {
    let name = if multi {
        format!("aisix:batch:{raw_batch_id}:{idx}")
    } else {
        format!("aisix:batch:{raw_batch_id}")
    };
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, name.as_bytes()).to_string()
}

/// 写进提交给上游的请求体里，见 [`BATCH_SUBMITTER_KEY`]。
fn stamp_batch_submitter(req: &mut Value, api_key_id: &str, model: &str) {
    let Some(obj) = req.as_object_mut() else {
        return;
    };
    let metadata = obj.entry("metadata").or_insert(Value::Null);
    if metadata.is_null() {
        *metadata = Value::Object(serde_json::Map::new());
    }
    let Some(metadata) = metadata.as_object_mut() else {
        return;
    };
    metadata.remove(BATCH_SUBMITTER_KEY);
    metadata.remove(BATCH_MODEL_KEY);
    if metadata.len() + 2 <= BATCH_METADATA_MAX_PAIRS {
        metadata.insert(BATCH_SUBMITTER_KEY.into(), api_key_id.into());
        metadata.insert(BATCH_MODEL_KEY.into(), model.into());
    }
}

/// 这个批次标记的提交者，密钥还在的话。
fn batch_submitter(snap: &aisix_core::AisixSnapshot, batch: &Value) -> Option<AuthenticatedKey> {
    let id = batch.get("metadata")?.get(BATCH_SUBMITTER_KEY)?.as_str()?;
    let entry = snap.apikeys.get_by_id(id)?;
    Some(AuthenticatedKey::new(snap, entry, None, false))
}

/// 给这个批次定价的模型行：提交时标记的那一行，否则是这次请求路由到的那一行。
/// 只认同一把上游密钥下的行 —— 批次就是在那把密钥名下跑的。
fn batch_priced_model(
    snap: &aisix_core::AisixSnapshot,
    batch: &Value,
    target: &JobTarget,
) -> Arc<ResourceEntry<Model>> {
    batch
        .get("metadata")
        .and_then(|m| m.get(BATCH_MODEL_KEY))
        .and_then(Value::as_str)
        .and_then(|name| snap.models.get_by_name(name))
        .filter(|row| row.value.provider_key_id.as_deref() == Some(&*target.pk_entry.id))
        .unwrap_or_else(|| Arc::clone(&target.model_entry))
}

/// 批次结束的时刻（上游的 unix 秒）。过期和取消的批次同样可能带部分结果 ——
/// 已经跑完的行上游照样收钱，只认 `completed` 的话，提交之后赶在完成前取消就
/// 成了免费的办法。别的状态返回外层 `None`：还没到要结算的时候。
fn batch_ended_at(batch: &Value) -> Option<Option<f64>> {
    let field = match batch.get("status").and_then(Value::as_str)? {
        "completed" => "completed_at",
        "expired" => "expired_at",
        "cancelled" => "cancelled_at",
        _ => return None,
    };
    Some(batch.get(field).and_then(Value::as_f64))
}

/// 这个批次的花费能不能记进计数器。
///
/// 一次性标记只跟计数器同寿（[`aisix_ratelimit::Limiter::claims_outlive_process`]），
/// 而按这个计数器扣款的账本是持久的：进程内存储下，重启之后再看到同一个批次，标记
/// 是空的、新进程的计数器涨上去，账本就把这批再扣一遍。所以这种存储下，结束得比本
/// 进程还早的批次一律不记：它多半已经被前一个进程记过，真没记过的那些（结束在停机
/// 窗口里）漏一笔，比多收一笔好 —— 漏记还能从用量事件里补，多收不能。
fn batch_credit_is_ruled_out(state: &ProxyState, ended_at: Option<f64>) -> bool {
    !state.limiter.claims_outlive_process()
        && ended_at.is_some_and(|t| t < aisix_obs::metrics::process_start_time_seconds())
}

/// 一次批次归因要用到的全部东西，在观察到结束的那一刻定下来。
struct BatchAttribution {
    state: ProxyState,
    target: JobTarget,
    /// 付钱的那把密钥：提交者，认不出提交者时是观察者。
    payer: AuthenticatedKey,
    model: Arc<ResourceEntry<Model>>,
    price: Option<aisix_core::models::model::ModelCost>,
    /// 付钱的那把密钥在定价模型上的花费桶。批处理的钱是事后落账的，那时没有
    /// 任何预留可以取键。
    spend_keys: Vec<String>,
    raw_batch_id: String,
    output_file_id: String,
    ended_at: Option<f64>,
    request_id: String,
    /// 观察到它的那次请求的头。观察者不是付钱的人时不带：那些头描述的是别人。
    request_headers: std::collections::BTreeMap<String, String>,
    user_agent: String,
}

/// 第一次观察到一个结束了、带结果文件的批次：记进进程内的集合（本进程只归因一次），
/// 把归因要用的东西定下来。不该归因时返回 `None`。Re-emission from a restarted
/// process or another replica is deduplicated by the control plane on the
/// deterministic `request_id` (see [`batch_attribution_request_id`]).
fn prepare_batch_attribution(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    // The request that observed the end.
    client: &ClientContext,
    target: &JobTarget,
    raw_batch_id: &str,
    batch: &Value,
) -> Option<BatchAttribution> {
    let ended_at = batch_ended_at(batch)?;
    let output_file_id = batch
        .get("output_file_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?
        .to_string();
    if require_safe_upstream_id(&output_file_id).is_err() {
        tracing::warn!(batch_id = %raw_batch_id, "batch output_file_id failed charset guard; skipping attribution");
        return None;
    }
    if !crate::state::remember_billed_batch(
        &state.billed_batches,
        raw_batch_id,
        std::time::Instant::now(),
        crate::state::BILLED_BATCH_DEDUP_WINDOW,
    ) {
        return None; // already attributed by this process
    }

    let snap = state.snapshot.load();
    let (payer, request_headers, user_agent) = match batch_submitter(&snap, batch) {
        Some(submitter) if submitter.entry.id != auth.entry.id => {
            (submitter, Default::default(), String::new())
        }
        _ => (
            auth.clone(),
            client.request_headers.clone(),
            client.user_agent.clone(),
        ),
    };
    // Through the same index `least_cost` ranks with: `pricing_key` first,
    // inline `cost` second. The batch is priced at what the model costs when
    // its output is collected.
    let model = batch_priced_model(&snap, batch, target);
    let price = state
        .pricing
        .for_snapshot(&snap)
        .resolve(&model.value)
        .cloned();
    let spend_keys = crate::quota::spend_bucket_keys(
        state,
        &snap,
        &payer,
        &crate::quota::ModelRateLimit::from_model(
            &model.value.display_name,
            &model.id,
            &model.value,
        ),
    );
    Some(BatchAttribution {
        state: state.clone(),
        target: target.clone(),
        payer,
        model,
        price,
        spend_keys,
        raw_batch_id: raw_batch_id.to_string(),
        output_file_id,
        ended_at,
        request_id: client.request_id.clone(),
        request_headers,
        user_agent,
    })
}

/// 请求路径上观察到批次结束：归因放到后台去，这次应答不等它。
fn maybe_attribute_batch(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    target: &JobTarget,
    raw_batch_id: &str,
    batch: &Value,
) {
    let Some(job) = prepare_batch_attribution(state, auth, client, target, raw_batch_id, batch)
    else {
        return;
    };
    tokio::spawn(async move {
        if let Err(e) = attribute_batch_usage(&job).await {
            // Release the guard so a later retrieve can retry the download.
            job.state.billed_batches.remove(&job.raw_batch_id);
            tracing::warn!(
                batch_id = %job.raw_batch_id,
                error = %e,
                "batch usage attribution failed; will retry on next retrieve",
            );
        }
    });
}

/// 列表也能看到批次结束：只查列表、再按 `output_file_id` 下结果文件的调用方永远
/// 走不到按 id 读的那条路。
///
/// 只认带提交者标记、而且提交者还在的行：列表里是这把上游密钥名下所有调用方的批次，
/// 没有标记的那些说不清该谁付，不能算到正在列表的这把密钥头上。
fn attribute_listed_batches(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    target: &JobTarget,
    bytes: &[u8],
) {
    let Ok(list) = serde_json::from_slice::<Value>(bytes) else {
        return;
    };
    let snap = state.snapshot.load();
    for batch in list["data"].as_array().into_iter().flatten() {
        let Some(raw_id) = batch["id"].as_str() else {
            continue;
        };
        if require_safe_upstream_id(raw_id).is_err() || batch_submitter(&snap, batch).is_none() {
            continue;
        }
        // 列表回头会看到同一批：记不进计数器的就不必再下载一遍结果文件。
        if batch_ended_at(batch).is_some_and(|t| batch_credit_is_ruled_out(state, t)) {
            continue;
        }
        maybe_attribute_batch(state, auth, client, target, raw_id, batch);
    }
}

/// 网关自己盯着它创建的批次，直到结束：调用方可能根本不来读（下载结果用不着按 id
/// 读批次），而上游照样收钱。
///
/// 只活在这个进程里。重启之后还没结束的批次没人盯，由按 id 读和列表兜底。
fn watch_batch(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    target: &JobTarget,
    raw_batch_id: &str,
) {
    if require_safe_upstream_id(raw_batch_id).is_err() {
        return;
    }
    let (state, auth, client, target, raw_batch_id) = (
        state.clone(),
        auth.clone(),
        client.clone(),
        target.clone(),
        raw_batch_id.to_string(),
    );
    let span = tracing::Span::current();
    tokio::spawn(tracing::Instrument::instrument(
        async move {
            let started = Instant::now();
            while started.elapsed() < BATCH_WATCH_LIMIT {
                tokio::time::sleep(state.batch_watch_interval).await;
                let Some(batch) = poll_batch(&target, &raw_batch_id, &client.request_id).await
                else {
                    continue;
                };
                match batch.get("status").and_then(Value::as_str) {
                    Some("completed" | "expired" | "cancelled") => {
                        let Some(job) = prepare_batch_attribution(
                            &state,
                            &auth,
                            &client,
                            &target,
                            &raw_batch_id,
                            &batch,
                        ) else {
                            // 没有结果文件，或者别的路径已经在归因了。
                            return;
                        };
                        match attribute_batch_usage(&job).await {
                            Ok(()) => return,
                            Err(e) => {
                                state.billed_batches.remove(&raw_batch_id);
                                tracing::warn!(
                                    batch_id = %raw_batch_id,
                                    error = %e,
                                    "batch usage attribution failed; retrying",
                                );
                            }
                        }
                    }
                    Some("failed") => return,
                    _ => {}
                }
            }
        },
        span,
    ));
}

/// 读一次批次的当前状态。读不到不算什么：下一轮再读，不进熔断也不进部署指标。
async fn poll_batch(target: &JobTarget, raw_batch_id: &str, request_id: &str) -> Option<Value> {
    let url = upstream_url(target, &format!("/batches/{raw_batch_id}"), "").ok()?;
    let resp = upstream_request(target, reqwest::Method::GET, &url, request_id)
        .timeout(BATCH_ATTRIBUTION_TIMEOUT)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

/// Download the completed batch's output JSONL and emit aggregated
/// UsageEvents (one per provider-billed model appearing in the lines).
/// Per-line usage and cost, as mainstream gateways handle a completed batch.
async fn attribute_batch_usage(job: &BatchAttribution) -> Result<(), String> {
    let state = &job.state;
    let api_key_id = job.payer.entry.id.as_str();
    let jwt = job.payer.jwt.as_ref();
    let caller = &job.payer.caller();
    let model_id = job.model.id.as_str();
    let display_name = job.model.value.display_name.as_str();
    let cost = job.price.as_ref();
    let pk_id = job.target.pk_entry.id.as_str();
    let raw_batch_id = job.raw_batch_id.as_str();
    let spend_keys = &job.spend_keys;
    let request_headers = &job.request_headers;
    let user_agent = job.user_agent.as_str();

    let url = upstream_url(
        &job.target,
        &format!("/files/{}/content", job.output_file_id),
        "",
    )
    .map_err(|e| e.to_string())?;
    let resp = upstream_request(&job.target, reqwest::Method::GET, &url, &job.request_id)
        .timeout(BATCH_ATTRIBUTION_TIMEOUT)
        .send()
        .await
        .map_err(|e| aisix_gateway::transport_error_message(&e))?;
    if !resp.status().is_success() {
        return Err(format!("output file download returned {}", resp.status()));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| aisix_gateway::transport_error_message(&e))?;

    // Aggregate per provider-billed model (`response.body.model`).
    #[derive(Default)]
    struct Agg {
        prompt: u64,
        completion: u64,
        cached: u64,
        cache_write: u64,
        modality: aisix_gateway::ModalityTokens,
        total: crate::usage_attr::ReportedTotalSum,
        lines: u64,
        service_tier: TierAgreement,
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
        let cache_write = usage["prompt_tokens_details"]["cache_write_tokens"]
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
        agg.cache_write += cache_write;
        // Lines that counted nothing were skipped above.
        agg.total.add(usage, true);
        agg.modality =
            agg.modality
                .saturating_add(&aisix_gateway::ModalityTokens::from_openai_details(
                    usage.get("prompt_tokens_details"),
                    usage.get("completion_tokens_details"),
                ));
        agg.lines += 1;
        agg.service_tier.observe(resp_body["service_tier"].as_str());
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
    let provider_label = job
        .model
        .value
        .provider
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let configured_upstream = job
        .model
        .value
        .upstream_model()
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
    // 实例都只记一次。进程内存储下标记活不过重启，所以还要先看批次结束的时刻
    // （见 [`batch_credit_is_ruled_out`]）。TTL 取 30 天：供应商保留已完成批次的
    // 量级就是这个，之后不可能再被重取。
    //
    // 事件仍然无条件发 —— 它本来就该在重放时也到达消费端。
    let credit = if batch_credit_is_ruled_out(state, job.ended_at) {
        tracing::warn!(
            batch_id = %raw_batch_id,
            "batch ended before this process started and the rate-limit store does not \
             outlive a process: its spend is left uncounted rather than risk charging it twice",
        );
        false
    } else {
        state
            .limiter
            .claim_once(
                &format!("batch-spend:{raw_batch_id}"),
                std::time::Duration::from_secs(30 * 24 * 3600),
            )
            .await
    };
    // 先回扣额度，再发事件与指标。顺序是有意的：观察者只能看见事件和指标，
    // 把额度放在它们之后，就存在一个「指标已到、额度未到」的窗口，任何据此
    // 判断的检查都会间歇性地看错。
    //
    // 一处算价，两处用（额度那笔、事件那笔）。上一版把公式抄了两遍 —— 那正是
    // 两个数字会各自漂走的形状。
    //
    // 输出行里的 `prompt_tokens_details.cached_tokens` 有自己的费率
    // （`cached_input_per_1k`）：按全价输入算会多收，而事件里这个计数是照报的，
    // 于是按事件重算花费的控制面会得到另一个数字。
    let slice_cost_usd = |agg: &Agg| {
        cost.map(|c| {
            c.calculate_with_cache(
                crate::usage_attr::input_tokens_for_pricing(
                    agg.prompt,
                    agg.cached,
                    agg.cache_write,
                    0,
                    0,
                ),
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
            // NO-GUARDRAIL-CHAIN: a retroactive billing row for work the
            // provider did inside a batch. There is no request here, so no
            // chain was ever resolved and nothing could have been bypassed.
            request_id,
            occurred_at: aisix_obs::UsageEvent::occurred_at_now(),
            model_id: model_id.to_string(),
            api_key_id: api_key_id.to_string(),
            requested_model: display_name.to_string(),
            // A batch dispatches to one direct model, which is the entry
            // its caller addressed. Taken from the batch's own record, not
            // looked up by name: the poll runs long after the request, and
            // the model may have been renamed since.
            requested_model_id: model_id.to_string(),
            prompt_tokens: agg.prompt.min(u32::MAX as u64) as u32,
            completion_tokens: agg.completion.min(u32::MAX as u64) as u32,
            total_tokens: agg.total.recorded(),
            cached_prompt_tokens: agg.cached.min(u32::MAX as u64) as u32,
            cache_write_tokens: (agg.cache_write > 0)
                .then(|| agg.cache_write.min(u32::MAX as u64) as u32),
            modality_tokens: agg.modality,
            status_code: 200,
            provider_model_version: provider_model.clone(),
            // This path bypasses the emission chokepoint, so the bound it
            // applies is applied here.
            service_tier: agg
                .service_tier
                .agreed()
                .map(|t| crate::chat::sanitize_tag(t.to_owned()))
                .filter(|t| !t.is_empty()),
            cost_usd: slice_cost_usd(agg),
            inbound_protocol: "batch".to_string(),
            // Set here rather than by the emit chokepoint: this path
            // deliberately bypasses it (no live request, so no trace
            // bundle), and both labels still come from one constant.
            operation: crate::operation::BATCH_COMPLETION.operation.to_string(),
            // Each slice has its own request id, so each is its own last row.
            is_final_attempt: true,
            request_headers: request_headers.clone(),
            ..Default::default()
        };
        crate::usage_attr::apply_pk_telemetry(&mut event, &pk);
        // Attribution names the identity that observed completion — the
        // same caller the event's api_key_id already reflects.
        crate::usage_attr::apply_caller_identity(&mut event, jwt, caller);
        let usage_model =
            crate::usage_attr::usage_event_model_label(&snap, &event.requested_model).into_owned();
        state.usage_sink.try_emit(
            crate::operation::BATCH_COMPLETION.handler,
            event.clone(),
            crate::usage_attr::usage_event_labels(&usage_model, &pk),
        );
        // A background poll attributes usage after the fact — there is no
        // live request and therefore no trace bundle; the exporter falls
        // back to the legacy flat span (#1279).
        let facts = std::sync::Arc::new(aisix_obs::SpanFacts {
            upstream: crate::span_facts::upstream_facts(&snap, &event, pk_id),
            ..Default::default()
        });
        state.otlp_fan_out.fan_out(
            &event,
            None,
            None,
            Some(&facts),
            exporters.generation(),
            exporters.iter().map(|e| &*e.value),
        );
        // 同一笔钱也要进 Prometheus 的 token/花费族。事件只进控制面的账本，
        // 而按花费序列求和扣款的那一侧看不到它 —— 于是一个批次可以消耗巨量
        // token 而用户余额分毫不动。
        //
        // 与额度同受 `credit` 管：这是个计数器，重放一次就多收一次。
        if credit {
            // 取付钱那把密钥本身，不按 id 回查快照：批次跑了几天，密钥可能已经被
            // 吊销，而这笔钱还是它的主人花的。
            let owned_caller = crate::request_metrics::Owned::from_auth(&job.payer);
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
                    cached: event.cached_prompt_tokens,
                    spend_usd: event.cost_usd,
                    client_type: state.client_classifier.classify(user_agent),
                    ..Default::default()
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
    /// 这个面判断一把密钥说哪种协议，必须和请求路径实际怎么分派一致：
    /// chat 能用的密钥这里也能用，chat 分派不出去的这里也不收。
    ///
    /// `Hub::dispatch_two_tier` 先按 `provider` 找厂商专用 bridge（服务端
    /// 注册的是 `openai` 与 `anthropic`），找不到才看 `adapter`。所以
    /// `provider: openai` 不论 `adapter` 写什么都说 OpenAI 协议；
    /// `provider: anthropic` 说 Anthropic，这个面不支持；其余 provider ——
    /// 包括 `azure-openai` —— 都要靠 `adapter`。
    #[test]
    fn protocol_follows_the_request_path_dispatch() {
        use super::supported_adapter;
        use aisix_core::models::Adapter;

        let pk = |provider: &str, adapter: Option<&str>| -> aisix_core::ProviderKey {
            let mut json = serde_json::json!({
                "display_name": "k",
                "secret": "sk-x",
                "provider": provider,
            });
            if let Some(a) = adapter {
                json["adapter"] = serde_json::Value::String(a.into());
            }
            serde_json::from_value(json).expect("fixture parses")
        };

        // provider 命中专用 bridge：以它为准，adapter 不参与。
        assert_eq!(
            supported_adapter(&pk("openai", None)),
            Some(Adapter::Openai)
        );
        assert_eq!(
            supported_adapter(&pk("openai", Some("anthropic"))),
            Some(Adapter::Openai),
            "chat 把这把密钥交给 OpenAI 专用 bridge，这里不能拒"
        );
        assert_eq!(supported_adapter(&pk("anthropic", Some("openai"))), None);
        // 没有专用 bridge：只看 adapter。
        assert_eq!(
            supported_adapter(&pk("azure-openai", None)),
            None,
            "chat 分派不出一把没写 adapter 的 azure-openai 密钥，这里也不收"
        );
        assert_eq!(
            supported_adapter(&pk("azure-openai", Some("azure-openai"))),
            Some(Adapter::AzureOpenai)
        );
        assert_eq!(
            supported_adapter(&pk("deepseek", Some("openai"))),
            Some(Adapter::Openai)
        );
        assert_eq!(supported_adapter(&pk("deepseek", None)), None);
        // 专用 bridge 按字面匹配，不去空白。
        assert_eq!(supported_adapter(&pk(" openai", None)), None);
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

    const PK_A: &str = "11111111-1111-1111-1111-11111111111a";
    const PK_B: &str = "11111111-1111-1111-1111-11111111111b";
    /// The secret [`openai_pk`] gives `PK_A`, which seals ids minted on it.
    const SECRET_A: &str = "sk-up-11111111-1111-1111-1111-11111111111a";

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
    fn a_batch_row_has_a_tier_only_when_every_line_agrees() {
        let fold = |lines: &[Option<&str>]| {
            let mut t = TierAgreement::default();
            for l in lines {
                t.observe(*l);
            }
            t.agreed().map(str::to_owned)
        };
        assert_eq!(fold(&[Some("flex"), Some("flex")]).as_deref(), Some("flex"));
        assert_eq!(fold(&[Some("flex"), Some("default")]), None);
        assert_eq!(fold(&[Some("flex"), None]), None);
        assert_eq!(fold(&[None, Some("flex")]), None);
        assert_eq!(fold(&[Some("")]), None);
        assert_eq!(fold(&[]), None);
    }

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

    fn routed(raw: &str, model: &str) -> RoutedId {
        RoutedId {
            raw: raw.to_string(),
            model: model.to_string(),
            owner: None,
            tag: None,
        }
    }

    #[test]
    fn routed_id_roundtrip() {
        let id = encode_routed_id("file-abc123", "jobs-a", None);
        assert!(id.starts_with(ROUTED_ID_PREFIX));
        assert_eq!(decode_routed_id(&id), Some(routed("file-abc123", "jobs-a")));
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
        multipart_body_with_file(boundary, model, br#"{"custom_id":"r1"}"#)
    }

    /// [`multipart_body`] with the `file` part's bytes chosen by the
    /// caller, so a test can upload something that is not valid UTF-8.
    fn multipart_body_with_file(boundary: &str, model: Option<&str>, file: &[u8]) -> Vec<u8> {
        multipart_body_with_purpose(boundary, model, Some("batch"), file)
    }

    /// [`multipart_body_with_file`] with the declared `purpose` chosen by
    /// the caller — `None` omits the field entirely.
    fn multipart_body_with_purpose(
        boundary: &str,
        model: Option<&str>,
        purpose: Option<&str>,
        file: &[u8],
    ) -> Vec<u8> {
        let mut b = Vec::new();
        if let Some(m) = model {
            b.extend_from_slice(
                format!(
                    "--{boundary}\r\ncontent-disposition: form-data; name=\"model\"\r\n\r\n{m}\r\n"
                )
                .as_bytes(),
            );
        }
        if let Some(p) = purpose {
            b.extend_from_slice(
                format!(
                    "--{boundary}\r\ncontent-disposition: form-data; name=\"purpose\"\r\n\r\n{p}\r\n"
                )
                .as_bytes(),
            );
        }
        b.extend_from_slice(
            format!(
                "--{boundary}\r\ncontent-disposition: form-data; name=\"file\"; filename=\"input.jsonl\"\r\ncontent-type: application/jsonl\r\n\r\n"
            )
            .as_bytes(),
        );
        b.extend_from_slice(file);
        b.extend_from_slice(b"\r\n");
        b.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        b
    }

    #[test]
    fn an_owner_round_trips_and_an_unowned_id_reads_as_before() {
        let owned = encode_routed_id("file-abc", "jobs-a", Some(("team:t1", SECRET_A)));
        let decoded = decode_routed_id(&owned).expect("an owned id decodes");
        assert_eq!(
            (
                decoded.raw.as_str(),
                decoded.model.as_str(),
                decoded.owner.as_deref()
            ),
            ("file-abc", "jobs-a", Some("team:t1"))
        );
        let tag = decoded.tag.expect("an owned id is sealed");
        assert!(owner_tag_matches(SECRET_A, "file-abc", "team:t1", &tag));
        // The seal names this raw id, this owner and this provider key's secret.
        assert!(!owner_tag_matches(SECRET_A, "file-abc", "team:t2", &tag));
        assert!(!owner_tag_matches(SECRET_A, "file-xyz", "team:t1", &tag));
        assert!(!owner_tag_matches(
            "another-secret",
            "file-abc",
            "team:t1",
            &tag
        ));
        // The shape every id minted before owners existed has.
        let legacy = format!(
            "{ROUTED_ID_PREFIX}{}",
            URL_SAFE_NO_PAD.encode("file-abc;model,jobs-a")
        );
        assert_eq!(legacy, encode_routed_id("file-abc", "jobs-a", None));
        assert_eq!(
            decode_routed_id(&legacy),
            Some(routed("file-abc", "jobs-a"))
        );
    }

    fn scoped_key(token: &str, id: &str, fields: Value) -> ResourceEntry<ApiKey> {
        let mut doc = serde_json::json!({
            "key_hash": ApiKey::hash_bearer(token),
            "allowed_models": ["*"],
        });
        for (k, v) in fields.as_object().unwrap() {
            doc[k] = v.clone();
        }
        ResourceEntry::new(id, serde_json::from_value(doc).unwrap(), 1)
    }

    /// One upstream account, three kinds of key on it. What the owner holds is
    /// its own; everyone else, the shared key included, finds no such id. A
    /// refused call reaches no upstream, so it cannot tell whether the id exists.
    #[tokio::test]
    async fn a_scoped_key_reaches_only_what_it_owns() {
        let upstream = MockServer::builder().start().await;
        Mock::given(wm_method("GET"))
            .and(path("/v1/files/file-a/content"))
            .respond_with(ResponseTemplate::new(200).set_body_string("team a's data"))
            .mount(&upstream)
            .await;
        Mock::given(wm_method("GET"))
            .and(path("/v1/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": []})))
            .mount(&upstream)
            .await;
        Mock::given(wm_method("GET"))
            .and(path("/v1/batches/batch-a"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "batch-a",
                "status": "in_progress",
                "input_file_id": "file-a",
                "output_file_id": "file-out",
            })))
            .mount(&upstream)
            .await;

        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.models.insert(model("m-a", "jobs-a", PK_A));
        snap.apikeys.insert(scoped_key(
            "sk-a",
            "k-a",
            serde_json::json!({"team_id": "t-a"}),
        ));
        snap.apikeys.insert(scoped_key(
            "sk-b",
            "k-b",
            serde_json::json!({"team_id": "t-b"}),
        ));
        snap.apikeys.insert(scoped_key(
            "sk-u",
            "k-u",
            serde_json::json!({"user_id": "u-1"}),
        ));
        snap.apikeys
            .insert(scoped_key("sk-shared", "k-s", serde_json::json!({})));
        let app = build_app(snap);
        let call = |token: &'static str, method: &'static str, uri: String| {
            let app = app.clone();
            async move {
                let req = Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                let status = resp.status();
                let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
                (status, String::from_utf8_lossy(&bytes).into_owned())
            }
        };

        let a_file = encode_routed_id("file-a", "jobs-a", Some(("team:t-a", SECRET_A)));
        let content = format!("/v1/files/{a_file}/content");
        let (status, body) = call("sk-a", "GET", content.clone()).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "team a's data"));

        let seen = || async { upstream.received_requests().await.unwrap().len() };
        let before = seen().await;
        for token in ["sk-b", "sk-u", "sk-shared"] {
            let (status, _) = call(token, "GET", content.clone()).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{token}");
        }
        // A raw id, and one with no owner, name nobody a scoped key can be.
        for uri in [
            "/v1/files/file-a/content?model=jobs-a".to_string(),
            format!(
                "/v1/files/{}/content",
                encode_routed_id("file-a", "jobs-a", None)
            ),
        ] {
            let (status, _) = call("sk-b", "GET", uri).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }
        // A key with a user and no team is its own owner.
        let (status, _) = call(
            "sk-u",
            "GET",
            format!(
                "/v1/files/{}/content",
                encode_routed_id("file-a", "jobs-a", Some(("user:u-2", SECRET_A)))
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Lists span the account: scoped keys are told so, by name of the reason.
        for uri in ["/v1/files", "/v1/batches", "/v1/fine_tuning/jobs"] {
            let (status, body) = call("sk-a", "GET", format!("{uri}?model=jobs-a")).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
            assert!(body.contains("listing is not available"), "{uri}: {body}");
        }
        assert_eq!(
            seen().await,
            before,
            "none of the above reached the upstream"
        );

        // A key with no owner, on a provider key that owned keys reach, has
        // neither the raw id nor the list: both would hand it their files. It
        // keeps the unowned ids it was handed.
        let (status, _) = call(
            "sk-shared",
            "GET",
            "/v1/files/file-a/content?model=jobs-a".into(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = call("sk-shared", "GET", "/v1/files?model=jobs-a".into()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, body) = call(
            "sk-shared",
            "GET",
            format!(
                "/v1/files/{}/content",
                encode_routed_id("file-a", "jobs-a", Some((UNOWNED_SEAL, SECRET_A)))
            ),
        )
        .await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "team a's data"));
        // An unsealed unowned id is anyone's to write around a raw id, so it
        // does not open here.
        let (status, _) = call(
            "sk-shared",
            "GET",
            format!(
                "/v1/files/{}/content",
                encode_routed_id("file-a", "jobs-a", None)
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // What an owned batch names is owned too.
        let a_batch = encode_routed_id("batch-a", "jobs-a", Some(("team:t-a", SECRET_A)));
        let (status, body) = call("sk-a", "GET", format!("/v1/batches/{a_batch}")).await;
        assert_eq!(status, StatusCode::OK);
        let v: Value = serde_json::from_str(&body).unwrap();
        for field in ["id", "input_file_id", "output_file_id"] {
            let decoded = decode_routed_id(v[field].as_str().unwrap()).unwrap();
            assert_eq!(decoded.owner.as_deref(), Some("team:t-a"), "{field}");
        }
    }

    /// An owner is sealed into the id: one re-issued under another owner, with
    /// a seal guessed or left off, reads as an id that does not exist on every
    /// call that addresses it, and the provider hears nothing.
    #[tokio::test]
    async fn an_id_re_issued_under_another_owner_does_not_open() {
        let upstream = MockServer::builder().start().await;
        Mock::given(wm_method("GET"))
            .and(path("/v1/files/file-a/content"))
            .respond_with(ResponseTemplate::new(200).set_body_string("team a's data"))
            .mount(&upstream)
            .await;
        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.models.insert(model("m-a", "jobs-a", PK_A));
        for (token, id, team) in [("sk-a", "k-a", "t-a"), ("sk-b", "k-b", "t-b")] {
            snap.apikeys
                .insert(scoped_key(token, id, serde_json::json!({"team_id": team})));
        }
        let app = build_app(snap);
        let call = |method: &'static str, uri: String, body: Option<Value>| {
            let app = app.clone();
            async move {
                let mut req = Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("authorization", "Bearer sk-b");
                let body = match body {
                    Some(v) => {
                        req = req.header("content-type", "application/json");
                        axum::body::Body::from(v.to_string())
                    }
                    None => axum::body::Body::empty(),
                };
                app.oneshot(req.body(body).unwrap()).await.unwrap().status()
            }
        };
        let a_file = encode_routed_id("file-a", "jobs-a", Some(("team:t-a", SECRET_A)));
        let raw = decode_routed_id(&a_file).unwrap().raw;
        let unsealed = format!(
            "{ROUTED_ID_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(format!("{raw};owner,team:t-b;model,jobs-a"))
        );
        let guessed = encode_routed_id(&raw, "jobs-a", Some(("team:t-b", "a-guessed-secret")));
        for forged in [unsealed, guessed] {
            assert_eq!(
                call("GET", format!("/v1/files/{forged}/content"), None).await,
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                call("DELETE", format!("/v1/files/{forged}"), None).await,
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                call(
                    "POST",
                    "/v1/batches".into(),
                    Some(serde_json::json!({"input_file_id": forged, "endpoint": "/v1/chat/completions"})),
                )
                .await,
                StatusCode::NOT_FOUND
            );
        }
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }

    /// Where no key with an owner reaches the provider key, a key without one
    /// lists and uses raw ids as it always did: a single-tenant deployment does
    /// not change.
    #[tokio::test]
    async fn a_single_tenant_deployment_keeps_raw_ids_and_lists() {
        let upstream = MockServer::builder().start().await;
        Mock::given(wm_method("GET"))
            .and(path("/v1/files/file-a/content"))
            .respond_with(ResponseTemplate::new(200).set_body_string("data"))
            .mount(&upstream)
            .await;
        Mock::given(wm_method("GET"))
            .and(path("/v1/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": []})))
            .mount(&upstream)
            .await;
        Mock::given(wm_method("GET"))
            .and(path("/v1/batches"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "batch-a", "status": "in_progress",
                          "metadata": {"aisix_api_key_id": "k-s", "aisix_model": "jobs-a", "note": "x"}}],
            })))
            .mount(&upstream)
            .await;
        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.provider_keys.insert(openai_pk(PK_B, &upstream.uri()));
        snap.models.insert(model("m-a", "jobs-a", PK_A));
        snap.models.insert(model("m-b", "jobs-b", PK_B));
        snap.apikeys
            .insert(scoped_key("sk-shared", "k-s", serde_json::json!({})));
        // An owned key that reaches only the other provider key.
        snap.apikeys.insert(scoped_key(
            "sk-t",
            "k-t",
            serde_json::json!({"team_id": "t", "allowed_models": ["jobs-b"]}),
        ));
        let app = build_app(snap);
        for uri in [
            "/v1/files/file-a/content?model=jobs-a",
            "/v1/files?model=jobs-a",
            "/v1/batches?model=jobs-a",
        ] {
            let req = Request::builder()
                .method("GET")
                .uri(uri)
                .header("authorization", "Bearer sk-shared")
                .body(axum::body::Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            // A list carries the caller's metadata, not the gateway's markers.
            let body = to_bytes(resp.into_body(), 65536).await.unwrap();
            let body = String::from_utf8_lossy(&body);
            assert!(!body.contains("aisix_api_key_id"), "{uri}: {body}");
            assert!(!body.contains("aisix_model"), "{uri}: {body}");
        }
    }

    /// A job created from a file is a read of that file.
    #[tokio::test]
    async fn a_job_cannot_be_created_from_someone_elses_file() {
        let upstream = MockServer::builder().start().await;
        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.models.insert(model("m-a", "jobs-a", PK_A));
        snap.apikeys.insert(scoped_key(
            "sk-b",
            "k-b",
            serde_json::json!({"team_id": "t-b"}),
        ));
        let app = build_app(snap);
        let a_file = encode_routed_id("file-a", "jobs-a", Some(("team:t-a", SECRET_A)));
        let b_file = encode_routed_id("file-b", "jobs-a", Some(("team:t-b", SECRET_A)));
        for (uri, body) in [
            (
                "/v1/batches",
                serde_json::json!({"input_file_id": a_file, "endpoint": "/v1/chat/completions"}),
            ),
            (
                "/v1/fine_tuning/jobs",
                serde_json::json!({"training_file": a_file, "model": "gpt-4o-mini"}),
            ),
            (
                "/v1/fine_tuning/jobs",
                serde_json::json!({"training_file": b_file, "validation_file": a_file, "model": "gpt-4o-mini"}),
            ),
            // A provider's own id names no owner, so it is not this key's.
            (
                "/v1/batches?model=jobs-a",
                serde_json::json!({"input_file_id": "file-a", "endpoint": "/v1/chat/completions"}),
            ),
            (
                "/v1/fine_tuning/jobs?model=jobs-a",
                serde_json::json!({"training_file": "file-a", "model": "gpt-4o-mini"}),
            ),
            (
                "/v1/fine_tuning/jobs",
                serde_json::json!({"training_file": b_file, "validation_file": "file-a", "model": "gpt-4o-mini"}),
            ),
        ] {
            let req = Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", "Bearer sk-b")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}: {body}");
        }
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn create_file_routes_by_form_model_and_encodes_response_id() {
        let upstream = MockServer::builder().start().await;
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
            Some(routed("file-abc", "jobs-a")),
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
        let upstream = MockServer::builder().start().await;
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

    /// 审查失败放行时，这段文本没有被处理过，所以它不进全量导出；真正跑过的
    /// 审查作为对照，文本照常导出。
    #[tokio::test]
    async fn a_failed_open_check_keeps_the_control_body_off_the_export() {
        const TEXT: &str = "failed-open-batch-text-93e5";
        for (script, exported) in [
            (crate::INPUT_CHECK_FAILS_OPEN, false),
            (crate::INPUT_CHECK_PASSES, true),
        ] {
            let upstream = MockServer::builder().start().await;
            Mock::given(wm_method("POST"))
                .and(path("/v1/batches"))
                .respond_with(
                    ResponseTemplate::new(500)
                        .set_body_json(serde_json::json!({"error": {"message": "an answer"}})),
                )
                .mount(&upstream)
                .await;
            let snap = AisixSnapshot::new();
            snap.provider_keys
                .insert(openai_pk(PK_A, &format!("{}/v1", upstream.uri())));
            snap.models.insert(model("m-a", "gpt-a", PK_A));
            snap.apikeys.insert(apikey_entry(&["*"]));
            crate::seed_input_script_row(&snap, script);
            let collector = crate::seed_full_content_collector(&snap).await;
            let app = build_app(snap);
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/batches?model=gpt-a")
                        .header("authorization", "Bearer sk-caller")
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(
                            serde_json::json!({
                                "input_file_id": "file-1",
                                "endpoint": "/v1/chat/completions",
                                "completion_window": "24h",
                                "metadata": {"note": TEXT},
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(response.status().is_server_error(), "{}", response.status());
            let sent = crate::exported_for(&collector, &response).await;
            assert_eq!(sent.contains(TEXT), exported, "script {script}: {sent}");
        }
    }

    #[tokio::test]
    async fn create_batch_decodes_input_file_id_and_routes_to_embedded_model() {
        // Two providers; the encoded file id names model B — the request
        // must land on upstream B with the RAW file id and no gateway
        // routing fields.
        let upstream_a = MockServer::builder().start().await;
        let upstream_b = MockServer::builder().start().await;
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

        let encoded = encode_routed_id("file-realB", "jobs-b", None);
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
            Some(routed("batch_777", "jobs-b"))
        );
        assert_eq!(
            decode_routed_id(v["input_file_id"].as_str().unwrap()),
            Some(routed("file-realB", "jobs-b"))
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
        let upstream = MockServer::builder().start().await;
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

        let encoded = encode_routed_id("batch_1", "jobs-a", None);
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
        let upstream = MockServer::builder().start().await;
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

        let encoded = encode_routed_id("batch_1", "jobs-a", None);
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
        let upstream = MockServer::builder().start().await;
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

        let encoded = encode_routed_id("batch_1", "jobs-a", None);
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

    /// 一把只属于「另一个人」的密钥：明文 `sk-other`。
    fn other_apikey_entry() -> ResourceEntry<ApiKey> {
        use sha2::{Digest, Sha256};
        let hash = format!("{:x}", Sha256::digest(b"sk-other"));
        let k: ApiKey = serde_json::from_str(&format!(
            r#"{{"key_hash":"{hash}","allowed_models":["*"]}}"#
        ))
        .unwrap();
        ResourceEntry::new("k-other", k, 1)
    }

    /// 一个结束了的批次和它的结果文件：一行 1000+1000 token。按 `priced_model`
    /// 的 $0.5/1k，整批是 $1，即 1_000_000 micro-USD。
    async fn mount_ended_batch(upstream: &MockServer, batch: Value) {
        let id = batch["id"].as_str().unwrap().to_string();
        Mock::given(wm_method("GET"))
            .and(path(format!("/v1/batches/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(batch))
            .mount(upstream)
            .await;
        let line = serde_json::json!({
            "id": "batch_req_1",
            "response": {"status_code": 200, "body": {
                "model": "gpt-4o",
                "usage": {"prompt_tokens": 1000, "completion_tokens": 1000}
            }}
        });
        Mock::given(wm_method("GET"))
            .and(path("/v1/files/file-out/content"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(format!("{line}\n"), "application/jsonl"),
            )
            .mount(upstream)
            .await;
    }

    fn ended_batch_snapshot(upstream: &MockServer) -> AisixSnapshot {
        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.models.insert(priced_model("m-a", "jobs-a", PK_A));
        snap.apikeys.insert(apikey_entry(&["*"]));
        snap.apikeys.insert(other_apikey_entry());
        snap
    }

    fn get_as(uri: String, key: &str) -> Request<axum::body::Body> {
        Request::builder()
            .method("GET")
            .uri(uri)
            .header("authorization", format!("Bearer {key}"))
            .body(axum::body::Body::empty())
            .unwrap()
    }

    /// 花费序列渐渐出现（归因在后台跑）：等它的 `(api_key_id, 值)` 落定。
    async fn spend_series(state: &crate::ProxyState) -> Vec<(String, u64)> {
        for _ in 0..60 {
            let rendered = state.metrics.render();
            let series: Vec<(String, u64)> = rendered
                .lines()
                .filter(|l| l.starts_with(aisix_obs::metrics::M_LLM_SPEND_MICRO_USD_TOTAL))
                .map(|l| {
                    let key = l
                        .split("api_key_id=\"")
                        .nth(1)
                        .and_then(|rest| rest.split('"').next())
                        .unwrap_or_default()
                        .to_string();
                    let value = l.rsplit(' ').next().unwrap().parse().unwrap();
                    (key, value)
                })
                .collect();
            if !series.is_empty() {
                return series;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Vec::new()
    }

    /// 看批次的是另一把密钥，付钱的还是提交它的那把 —— 否则知道 id 的人一读，
    /// 账就记到了他头上。
    #[tokio::test]
    async fn the_submitter_pays_for_a_batch_another_key_reads() {
        let upstream = MockServer::builder().start().await;
        mount_ended_batch(
            &upstream,
            serde_json::json!({
                "id": "batch_1",
                "status": "completed",
                "output_file_id": "file-out",
                "metadata": {BATCH_SUBMITTER_KEY: "k-1", BATCH_MODEL_KEY: "jobs-a"}
            }),
        )
        .await;
        let (app, _rx, state) = build_app_with_sink_and_state(ended_batch_snapshot(&upstream));

        let encoded = encode_routed_id("batch_1", "jobs-a", None);
        let resp = app
            .oneshot(get_as(format!("/v1/batches/{encoded}"), "sk-other"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            v["metadata"].get(BATCH_SUBMITTER_KEY).is_none(),
            "网关的标记不该出现在给调用方的应答里: {v}"
        );

        assert_eq!(
            spend_series(&state).await,
            vec![("k-1".to_string(), 1_000_000)]
        );
    }

    /// 读批次时换一个更便宜的 `?model=`（同一把上游密钥下）不能换一个价钱。
    #[tokio::test]
    async fn a_batch_is_priced_on_the_model_it_was_submitted_through() {
        let upstream = MockServer::builder().start().await;
        mount_ended_batch(
            &upstream,
            serde_json::json!({
                "id": "batch_1",
                "status": "completed",
                "output_file_id": "file-out",
                "metadata": {BATCH_SUBMITTER_KEY: "k-1", BATCH_MODEL_KEY: "jobs-a"}
            }),
        )
        .await;
        let snap = ended_batch_snapshot(&upstream);
        let cheap: Model = serde_json::from_str(&format!(
            r#"{{"display_name":"jobs-cheap","provider":"openai","model_name":"gpt-4o","provider_key_id":"{PK_A}","cost":{{"input_per_1k":0.0,"output_per_1k":0.0}}}}"#
        ))
        .unwrap();
        snap.models.insert(ResourceEntry::new("m-cheap", cheap, 1));
        let (app, _rx, state) = build_app_with_sink_and_state(snap);

        let resp = app
            .oneshot(get_as(
                "/v1/batches/batch_1?model=jobs-cheap".to_string(),
                "sk-caller",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            spend_series(&state).await,
            vec![("k-1".to_string(), 1_000_000)]
        );
    }

    /// 取消或过期的批次带着部分结果，已经跑完的行上游照样收钱。只认 `completed`
    /// 的话，赶在完成前取消就成了免费的办法。
    #[tokio::test]
    async fn a_cancelled_or_expired_batch_with_output_is_charged() {
        for status in ["cancelled", "expired"] {
            let upstream = MockServer::builder().start().await;
            mount_ended_batch(
                &upstream,
                serde_json::json!({
                    "id": "batch_1",
                    "status": status,
                    "output_file_id": "file-out"
                }),
            )
            .await;
            let (app, _rx, state) = build_app_with_sink_and_state(ended_batch_snapshot(&upstream));
            let encoded = encode_routed_id("batch_1", "jobs-a", None);
            let resp = app
                .oneshot(get_as(format!("/v1/batches/{encoded}"), "sk-caller"))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(
                spend_series(&state).await,
                vec![("k-1".to_string(), 1_000_000)],
                "{status}"
            );
        }
    }

    /// 只查列表、再下载结果文件的调用方走不到按 id 读的路。列表里带提交者标记的
    /// 批次记到提交者头上；没有标记的说不清是谁的，不能算到正在列表的这把密钥上。
    #[tokio::test]
    async fn a_listed_batch_is_charged_to_its_submitter_and_only_a_marked_one() {
        let upstream = MockServer::builder().start().await;
        mount_ended_batch(
            &upstream,
            serde_json::json!({"id": "batch_unused", "status": "completed"}),
        )
        .await;
        Mock::given(wm_method("GET"))
            .and(path("/v1/batches"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [
                    {
                        "id": "batch_mine",
                        "status": "completed",
                        "output_file_id": "file-out",
                        "metadata": {BATCH_SUBMITTER_KEY: "k-1", BATCH_MODEL_KEY: "jobs-a"}
                    },
                    {
                        "id": "batch_unmarked",
                        "status": "completed",
                        "output_file_id": "file-out"
                    },
                    {
                        "id": "batch_running",
                        "status": "in_progress",
                        "metadata": {BATCH_SUBMITTER_KEY: "k-1", BATCH_MODEL_KEY: "jobs-a"}
                    }
                ]
            })))
            .mount(&upstream)
            .await;
        let (app, _rx, state) = build_app_with_sink_and_state(ended_batch_snapshot(&upstream));

        let resp = app
            .oneshot(get_as("/v1/batches?model=jobs-a".to_string(), "sk-other"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            spend_series(&state).await,
            vec![("k-1".to_string(), 1_000_000)],
            "只有带标记的那一批入账，而且记在提交者头上"
        );
    }

    /// 调用方压根不读批次也要入账：结果可以按 `output_file_id` 直接下，而上游照样
    /// 收钱。网关盯着自己创建的批次，不靠任何一次后续请求。
    #[tokio::test]
    async fn a_created_batch_is_charged_when_nobody_ever_reads_it() {
        let upstream = MockServer::builder().start().await;
        Mock::given(wm_method("POST"))
            .and(path("/v1/batches"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "batch_1",
                "object": "batch",
                "status": "validating"
            })))
            .mount(&upstream)
            .await;
        // 没带标记的上游（调用方写满了 metadata，或上游不回显）：盯的时候用的是
        // 提交那一刻的身份。
        mount_ended_batch(
            &upstream,
            serde_json::json!({
                "id": "batch_1",
                "status": "completed",
                "output_file_id": "file-out"
            }),
        )
        .await;

        let (tx, _rx) = tokio::sync::mpsc::channel::<ObsUsageEvent>(32);
        let state = crate::ProxyState::new(
            SnapshotHandle::new(ended_batch_snapshot(&upstream)),
            Arc::new(Hub::new()),
            &cfg(),
        )
        .without_cache()
        .with_usage_sink(UsageSink::new(tx))
        .with_batch_watch_interval(Duration::from_millis(20));
        let app = crate::build_router(state.clone());

        let body = serde_json::json!({
            "input_file_id": encode_routed_id("file-in", "jobs-a", None),
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
        assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);

        assert_eq!(
            spend_series(&state).await,
            vec![("k-1".to_string(), 1_000_000)]
        );
    }

    /// 提交者与定价模型记到上游的批次上；调用方自己写的同名键不算数，这样才伪造
    /// 不了别人来付钱。它写的其它 metadata 原样留着，而给它的应答里看不到标记。
    #[tokio::test]
    async fn a_created_batch_carries_its_submitter_upstream_and_a_forged_one_is_replaced() {
        let upstream = MockServer::builder().start().await;
        Mock::given(wm_method("POST"))
            .and(path("/v1/batches"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "batch_1",
                "status": "validating",
                "metadata": {"note": "x", BATCH_SUBMITTER_KEY: "k-1", BATCH_MODEL_KEY: "jobs-a"}
            })))
            .mount(&upstream)
            .await;
        let (app, _rx, _state) = build_app_with_sink_and_state(ended_batch_snapshot(&upstream));

        let body = serde_json::json!({
            "input_file_id": encode_routed_id("file-in", "jobs-a", None),
            "endpoint": "/v1/chat/completions",
            "completion_window": "24h",
            "metadata": {"note": "x", BATCH_SUBMITTER_KEY: "k-other", BATCH_MODEL_KEY: "free"}
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

        let received = upstream.received_requests().await.unwrap();
        let sent: Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(
            sent["metadata"],
            serde_json::json!({"note": "x", BATCH_SUBMITTER_KEY: "k-1", BATCH_MODEL_KEY: "jobs-a"})
        );

        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["metadata"], serde_json::json!({"note": "x"}), "{v}");
    }

    /// 上游的 metadata 有键值对上限：调用方已经写满时不打标记，请求照样发得出去。
    #[test]
    fn a_full_metadata_is_left_alone() {
        let mut full = serde_json::Map::new();
        for i in 0..BATCH_METADATA_MAX_PAIRS {
            full.insert(format!("k{i}"), Value::String("v".into()));
        }
        let mut req = serde_json::json!({"metadata": full});
        stamp_batch_submitter(&mut req, "k-1", "jobs-a");
        assert_eq!(
            req["metadata"].as_object().unwrap().len(),
            BATCH_METADATA_MAX_PAIRS
        );
        assert!(req["metadata"].get(BATCH_SUBMITTER_KEY).is_none());

        // 留得下两个键才打；null 当成没写。
        let mut req = serde_json::json!({"metadata": null});
        stamp_batch_submitter(&mut req, "k-1", "jobs-a");
        assert_eq!(req["metadata"][BATCH_SUBMITTER_KEY], "k-1");
    }

    /// 进程内存储的一次性标记活不过重启，而账本活得过：重启之后再读同一个批次，
    /// 新进程的计数器涨一笔，账本就再扣一遍。结束得比本进程还早的批次不再进计数器，
    /// 用量事件照发（消费端按确定性 id 去重）。
    #[tokio::test]
    async fn a_batch_that_ended_before_this_process_started_is_not_charged_again() {
        let upstream = MockServer::builder().start().await;
        mount_ended_batch(
            &upstream,
            serde_json::json!({
                "id": "batch_1",
                "status": "completed",
                "completed_at": 1_000_000_000,
                "output_file_id": "file-out"
            }),
        )
        .await;
        let (app, mut rx, state) = build_app_with_sink_and_state(ended_batch_snapshot(&upstream));
        assert!(
            !state.limiter.claims_outlive_process(),
            "这条用例说的就是进程内存储"
        );

        let encoded = encode_routed_id("batch_1", "jobs-a", None);
        let resp = app
            .oneshot(get_as(format!("/v1/batches/{encoded}"), "sk-caller"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let mut saw_batch_event = false;
        for _ in 0..40 {
            match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
                Ok(Some(ev)) if ev.inbound_protocol == "batch" => {
                    saw_batch_event = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(saw_batch_event, "事件本来就是幂等的，该发还是发");
        let rendered = state.metrics.render();
        assert!(
            !rendered.contains(aisix_obs::metrics::M_LLM_SPEND_MICRO_USD_TOTAL),
            "前一个进程可能已经记过的批次又进了计数器: {rendered}"
        );
    }

    /// 反过来，本进程启动之后才结束的批次照常入账 —— 上面那条闸不能把正常的账挡掉。
    #[tokio::test]
    async fn a_batch_that_ended_after_this_process_started_is_charged() {
        let upstream = MockServer::builder().start().await;
        mount_ended_batch(
            &upstream,
            serde_json::json!({
                "id": "batch_1",
                "status": "completed",
                "completed_at": aisix_obs::metrics::process_start_time_seconds() + 1.0,
                "output_file_id": "file-out"
            }),
        )
        .await;
        let (app, _rx, state) = build_app_with_sink_and_state(ended_batch_snapshot(&upstream));
        let encoded = encode_routed_id("batch_1", "jobs-a", None);
        let resp = app
            .oneshot(get_as(format!("/v1/batches/{encoded}"), "sk-caller"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            spend_series(&state).await,
            vec![("k-1".to_string(), 1_000_000)]
        );
    }

    #[tokio::test]
    async fn completed_batch_retrieve_attributes_usage_once() {
        let upstream = MockServer::builder().start().await;
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
                "service_tier": "default",
                "usage": {"prompt_tokens": 10, "completion_tokens": 5,
                           "prompt_tokens_details": {"cached_tokens": 2}}
            }}
        });
        let line2 = serde_json::json!({
            "id": "batch_req_2",
            "custom_id": "r2",
            "response": {"status_code": 200, "body": {
                "model": "gpt-4o-2024-08-06",
                "service_tier": "default",
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

        let encoded = encode_routed_id("batch_1", "jobs-a", None);
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
            Some(routed("file-out", "jobs-a"))
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
        assert!(agg.is_final_attempt, "a batch row stands for itself");
        assert_eq!(agg.prompt_tokens, 17);
        assert_eq!(agg.completion_tokens, 8);
        assert_eq!(agg.cached_prompt_tokens, 2);
        assert_eq!(agg.provider_model_version, "gpt-4o-2024-08-06");
        // Every line of the row reported the same tier.
        assert_eq!(agg.service_tier.as_deref(), Some("default"));
        assert_eq!(agg.requested_model, "jobs-a");

        // Every later retrieve: management event only — this process
        // attributes a batch once for as long as it runs.
        for _ in 0..3 {
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
    }

    // ---- fine-tuning ----

    #[tokio::test]
    async fn ft_create_routes_by_training_file_and_reencodes_ids() {
        let upstream = MockServer::builder().start().await;
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
            "training_file": encode_routed_id("file-train", "jobs-a", None),
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
            Some(routed("ftjob-9", "jobs-a"))
        );
        assert_eq!(
            decode_routed_id(v["result_files"][0].as_str().unwrap()),
            Some(routed("file-res", "jobs-a"))
        );
    }

    // ---- azure ----

    #[tokio::test]
    async fn azure_adapter_uses_api_key_header_and_resource_scoped_path() {
        let upstream = MockServer::builder().start().await;
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

        let encoded = encode_routed_id("batch_az", "jobs-az", None);
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

    /// A key restricted to provider groups reaches this surface only through
    /// a provider key in one of them: a named model outside them is refused
    /// as a routing group with no target left is, and the default pick passes
    /// over such a model rather than landing on it.
    #[test]
    fn a_key_with_groups_targets_only_models_inside_them() {
        let snap = AisixSnapshot::new();
        for (id, group) in [(PK_A, "grp-alpha"), (PK_B, "grp-beta")] {
            let pk: aisix_core::ProviderKey = serde_json::from_value(serde_json::json!({
                "display_name": format!("pk-{group}"), "secret": "sk-up",
                "api_base": "http://unused", "provider": "openai", "adapter": "openai",
                "groups": [group],
            }))
            .unwrap();
            snap.provider_keys.insert(ResourceEntry::new(id, pk, 1));
        }
        // The out-of-group model has the lower id, so it is the default a
        // key without groups lands on.
        snap.models.insert(model("m-a", "jobs-alpha", PK_A));
        snap.models.insert(model("m-b", "jobs-beta", PK_B));
        let target = |groups: &[&str], wanted: Option<&str>| {
            let key: ApiKey = serde_json::from_value(serde_json::json!({
                "key_hash": "h", "allowed_models": ["*"],
                "allowed_provider_groups": groups,
            }))
            .unwrap();
            let auth = AuthenticatedKey::new(
                &snap,
                Arc::new(ResourceEntry::new("k-1", key, 1)),
                None,
                false,
            );
            resolve_target(&snap, &auth, wanted, &ClientContext::default())
                .map(|t| t.model_entry.value.display_name.clone())
        };

        assert_eq!(target(&[], None).unwrap(), "jobs-alpha");
        assert_eq!(target(&["grp-beta"], None).unwrap(), "jobs-beta");
        assert_eq!(
            target(&["grp-beta"], Some("jobs-beta")).unwrap(),
            "jobs-beta"
        );
        let err = target(&["grp-beta"], Some("jobs-alpha")).unwrap_err();
        assert!(
            matches!(
                err,
                ProxyError::AllCandidatesUnavailable {
                    retry_after_secs: None
                }
            ),
            "{err:?}"
        );
        assert!(!format!("{err} {err:?}").contains("grp-"), "{err:?}");
        // With nothing inside the groups there is no default to pick.
        assert!(matches!(
            target(&["grp-zeta"], None),
            Err(ProxyError::InvalidRequest(_))
        ));
    }

    /// With no model named, the row a jobs request lands on picks the provider
    /// key it is sent with, so it has to be the same on every replica: the
    /// model table iterates in no fixed order.
    #[tokio::test]
    async fn the_default_target_does_not_depend_on_table_order() {
        let chosen = MockServer::builder().start().await;
        let other = MockServer::builder().start().await;
        for server in [&chosen, &other] {
            Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/v1/batches"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": []})),
                )
                .mount(server)
                .await;
        }
        for _ in 0..32 {
            let snap = AisixSnapshot::new();
            snap.provider_keys.insert(openai_pk(PK_A, &chosen.uri()));
            snap.provider_keys.insert(openai_pk(PK_B, &other.uri()));
            snap.models.insert(model("m-1", "first", PK_A));
            snap.models.insert(model("m-2", "second", PK_B));
            snap.apikeys.insert(apikey_entry(&["*"]));
            let resp = build_app(snap)
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/v1/batches")
                        .header("authorization", "Bearer sk-caller")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
        assert_eq!(chosen.received_requests().await.unwrap().len(), 32);
        assert!(other.received_requests().await.unwrap().is_empty());
    }

    /// 这条路和 /v1/chat/completions 花的是同一个 key 级窗口，所以也得照实
    /// 汇报——头给调用方，gauge 给运维。
    ///
    /// 源码扫描（`endpoint_family_parity`）只能证明「调用点在」，证明不了
    /// 「头真的落到响应上、series 真的出现在 /metrics 里」，所以这里驱动一次
    /// 真实请求来断言可观测到的那一面。
    #[tokio::test]
    async fn a_job_route_reports_the_window_it_spent() {
        let upstream = MockServer::builder().start().await;
        Mock::given(wm_method("POST"))
            .and(path("/v1/batches"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "batch_rl",
                "object": "batch",
                "status": "validating",
                "input_file_id": "file-in"
            })))
            .mount(&upstream)
            .await;

        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.models.insert(model("m-a", "jobs-a", PK_A));
        // 不限速的 key 没有窗口可报（`peek` 返回 None），得给它一个 rpm。
        let k: ApiKey = serde_json::from_str(
            r#"{"key_hash":"8b6712790a2089c67aa97a2d80022df18cc65c7814350e33baebe79aab508891","allowed_models":["*"],"rate_limit":{"rpm":10}}"#,
        )
        .unwrap();
        snap.apikeys.insert(ResourceEntry::new("k-1", k, 1));
        let (app, _rx, state) = build_app_with_sink_and_state(snap);

        let body = serde_json::json!({
            "input_file_id": encode_routed_id("file-in", "jobs-a", None),
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

        let hdr = |n: &str| {
            resp.headers()
                .get(n)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        assert_eq!(
            hdr("x-ratelimit-limit-requests").as_deref(),
            Some("10"),
            "the caller must be told the ceiling this request counted against"
        );
        assert_eq!(
            hdr("x-ratelimit-remaining-requests").as_deref(),
            Some("9"),
            "one batch request out of an rpm of 10 leaves 9; anything else \
             means the number the caller schedules back-off from is wrong"
        );

        let rendered = state.metrics.render();
        let line = rendered
            .lines()
            .find(|l| l.starts_with("aisix_ratelimit_remaining_requests{"))
            .unwrap_or_else(|| {
                panic!("remaining-requests gauge missing after a job request:\n{rendered}")
            });
        assert!(
            line.contains(r#"api_key_id="k-1""#) && line.contains(r#"model="jobs-a""#),
            "gauge must carry the caller and the resolved model row; got {line}"
        );
        assert!(
            line.ends_with(" 9"),
            "gauge must show what the job request left, not a stale reading; got {line}"
        );
    }
    /// A `mask` rule must rewrite the uploaded batch file before it leaves
    /// the gateway. The file IS the end-user content on this surface — the
    /// prompts a batch will run — so a mask rule that is accepted, resolved
    /// and reported as applied while the bytes go upstream verbatim is the
    /// #932 half-wiring, one endpoint later.
    #[tokio::test]
    async fn a_mask_rule_rewrites_the_uploaded_file_before_it_leaves() {
        let upstream = MockServer::builder().start().await;
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
        let g: aisix_core::Guardrail = serde_json::from_str(
            r#"{"name":"mask-email","enabled":true,"hook_point":"input","kind":"pii","detectors":[{"type":"email","action":"mask"}]}"#,
        )
        .unwrap();
        crate::seed_env_scoped_guardrail(&snap, ResourceEntry::new("g-mask", g, 1));
        let (app, mut rx) = build_app_with_sink(snap);

        let boundary = "XBOUNDARYX";
        let mut b: Vec<u8> = Vec::new();
        b.extend_from_slice(
            format!(
                "--{boundary}\r\ncontent-disposition: form-data; name=\"model\"\r\n\r\njobs-a\r\n"
            )
            .as_bytes(),
        );
        b.extend_from_slice(
            format!(
                "--{boundary}\r\ncontent-disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n"
            )
            .as_bytes(),
        );
        b.extend_from_slice(
            format!(
                "--{boundary}\r\ncontent-disposition: form-data; name=\"file\"; filename=\"input.jsonl\"\r\ncontent-type: application/jsonl\r\n\r\n{{\"custom_id\":\"r1\",\"body\":{{\"messages\":[{{\"role\":\"user\",\"content\":\"mail alice@example.com\"}}]}}}}\r\n"
            )
            .as_bytes(),
        );
        b.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

        let req = Request::builder()
            .method("POST")
            .uri("/v1/files")
            .header("authorization", "Bearer sk-caller")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(axum::body::Body::from(b))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "a mask rule must not block");

        let received = upstream.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let sent = String::from_utf8_lossy(&received[0].body).into_owned();
        assert!(
            !sent.contains("alice@example.com"),
            "the uploaded file reached the provider unmasked:\n{sent}"
        );
        // 其它字段照原样转发 —— 遮蔽不能顺手改坏这次上传的其它部分。
        assert!(
            sent.contains("name=\"purpose\""),
            "purpose must still forward"
        );
        assert!(
            sent.contains("filename=\"input.jsonl\""),
            "the file part must keep its filename"
        );
        assert!(
            sent.contains(r#""custom_id":"r1""#),
            "masking must rewrite the matched span only, not the whole file:\n{sent}"
        );

        // 遮蔽了却不在用量事件里留痕，等于没人能审计。
        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must be emitted")
            .expect("usage_sink sender dropped");
        assert!(
            !event.redacted_entity_counts.is_empty(),
            "a mask that leaves no trace in the usage event cannot be audited"
        );
    }
    /// 在 email mask 规则下上传一个文件部件，返回状态码与上游收到的 body
    /// （没有转发时为 `None`）。
    async fn upload_bytes_under_email_mask(payload: &[u8]) -> (StatusCode, Option<Vec<u8>>) {
        let upstream = MockServer::builder().start().await;
        Mock::given(wm_method("POST"))
            .and(path("/v1/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "file-bin", "object": "file", "purpose": "batch", "filename": "b.bin"
            })))
            .mount(&upstream)
            .await;

        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.models.insert(model("m-a", "jobs-a", PK_A));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let g: aisix_core::Guardrail = serde_json::from_str(
            r#"{"name":"mask-email","enabled":true,"hook_point":"input","kind":"pii","detectors":[{"type":"email","action":"mask"}]}"#,
        )
        .unwrap();
        crate::seed_env_scoped_guardrail(&snap, ResourceEntry::new("g-mask", g, 1));
        let app = build_app(snap);

        let boundary = "XBINX";
        let mut b: Vec<u8> = Vec::new();
        b.extend_from_slice(
            format!(
                "--{boundary}\r\ncontent-disposition: form-data; name=\"model\"\r\n\r\njobs-a\r\n"
            )
            .as_bytes(),
        );
        b.extend_from_slice(
            format!("--{boundary}\r\ncontent-disposition: form-data; name=\"file\"; filename=\"b.bin\"\r\n\r\n")
                .as_bytes(),
        );
        b.extend_from_slice(payload);
        b.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let req = Request::builder()
            .method("POST")
            .uri("/v1/files")
            .header("authorization", "Bearer sk-caller")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(axum::body::Body::from(b))
            .unwrap();
        let status = app.oneshot(req).await.unwrap().status();
        let sent = upstream
            .received_requests()
            .await
            .unwrap()
            .first()
            .map(|r| r.body.clone());
        (status, sent)
    }

    /// 遮蔽是在文本 span 上做替换。一个不是合法 UTF-8 的上传只查不改 ——
    /// 把 lossy 转换的结果写回去会把文件毁掉，而"护栏开着就悄悄改坏用户
    /// 上传的文件"比漏遮蔽更糟。遮蔽规则命中不了它时，它逐字节原样转发。
    #[tokio::test]
    async fn a_non_utf8_upload_is_checked_but_relayed_byte_for_byte() {
        // 0xff / 0xfe 在 UTF-8 里永远非法。
        let payload: &[u8] = b"\xff\xfe\x00bin nothing to mask here";
        let (status, sent) = upload_bytes_under_email_mask(payload).await;
        assert_eq!(status, StatusCode::OK);
        let sent = sent.expect("the upload reaches the provider");
        assert!(
            sent.windows(payload.len()).any(|w| w == payload),
            "a non-text upload must reach the provider byte-for-byte"
        );
    }

    /// 改写不了也不能不经遮蔽就转发：一个非法字节就能让一整份 PII 原样过去。
    /// 遮蔽规则命中一个不是合法 UTF-8 的上传 —— 原样写出的、或 JSON 转义写出
    /// 的 —— 时拒绝上传。
    #[tokio::test]
    async fn a_non_utf8_upload_a_mask_rule_matches_is_refused() {
        for payload in [
            b"\xff\xfe\x00bin alice@example.com".as_slice(),
            // A line no strict parse reads cannot have its strings rewritten.
            b"{\"custom_id\":\"r1\",\"x\":NaN,\"body\":{\"content\":\"alice\\u0040example.com\"}}\n".as_slice(),
            b"{\"custom_id\":\"r1\",\"body\":{\"messages\":[{\"role\":\"user\",\"content\":\"alice\\u0040example.com\"}]}}\n\xff\n"
                .as_slice(),
        ] {
            let (status, sent) = upload_bytes_under_email_mask(payload).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{payload:?}");
            assert!(sent.is_none(), "nothing may reach the provider: {payload:?}");
        }
    }
    /// 一个 body 里出现两个 `file` 部件时，被扫的和被改写的必须是同一个。
    ///
    /// 这条是给我自己改坏的地方兜底的：为了让遮蔽能落到文件上，表单从
    /// 「边读边拼」改成了「读完再拼」，中间版本扫第一个、却把改写后的字节
    /// 写进了每一个 —— 于是第二个部件被换成了第一个部件的内容。安静地毁掉
    /// 用户上传的东西，比漏遮蔽还糟。
    #[tokio::test]
    async fn two_file_parts_keep_the_scan_and_the_rewrite_on_the_same_one() {
        let upstream = MockServer::builder().start().await;
        Mock::given(wm_method("POST"))
            .and(path("/v1/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "file-two", "object": "file", "purpose": "batch", "filename": "b.jsonl"
            })))
            .mount(&upstream)
            .await;

        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.models.insert(model("m-a", "jobs-a", PK_A));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let g: aisix_core::Guardrail = serde_json::from_str(
            r#"{"name":"mask-email","enabled":true,"hook_point":"input","kind":"pii","detectors":[{"type":"email","action":"mask"}]}"#,
        )
        .unwrap();
        crate::seed_env_scoped_guardrail(&snap, ResourceEntry::new("g-mask", g, 1));
        let app = build_app(snap);

        let boundary = "XTWOX";
        let part = |name: &str, body: &str| {
            format!(
                "--{boundary}\r\ncontent-disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\r\n{body}\r\n"
            )
        };
        let mut b = String::new();
        b.push_str(&format!(
            "--{boundary}\r\ncontent-disposition: form-data; name=\"model\"\r\n\r\njobs-a\r\n"
        ));
        b.push_str(&part("first.jsonl", "FIRST-ONLY-MARKER"));
        b.push_str(&part("second.jsonl", "SECOND mail bob@example.com"));
        b.push_str(&format!("--{boundary}--\r\n"));

        let req = Request::builder()
            .method("POST")
            .uri("/v1/files")
            .header("authorization", "Bearer sk-caller")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(axum::body::Body::from(b))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let received = upstream.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let sent = String::from_utf8_lossy(&received[0].body).into_owned();
        // 被扫的那个（最后一个）遮蔽掉了……
        assert!(
            !sent.contains("bob@example.com"),
            "the scanned part must be masked:\n{sent}"
        );
        // ……另一个原封不动地转发，没有被顶替。
        assert!(
            sent.contains("FIRST-ONLY-MARKER"),
            "the other file part must survive untouched:\n{sent}"
        );
        assert!(
            sent.contains("filename=\"first.jsonl\"") && sent.contains("filename=\"second.jsonl\""),
            "both parts must still be present, with their own filenames:\n{sent}"
        );
    }
    async fn upload_with_guardrail(
        guardrail: &str,
        parts: &[(&str, &str)],
    ) -> (StatusCode, String) {
        let upstream = MockServer::builder().start().await;
        Mock::given(wm_method("POST"))
            .and(path("/v1/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "file-x", "object": "file", "purpose": "batch", "filename": "b.jsonl"
            })))
            .mount(&upstream)
            .await;
        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.models.insert(model("m-a", "jobs-a", PK_A));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let g: aisix_core::Guardrail = serde_json::from_str(guardrail).unwrap();
        crate::seed_env_scoped_guardrail(&snap, ResourceEntry::new("g-1", g, 1));
        let app = build_app(snap);
        let boundary = "XGRX";
        let mut b = format!(
            "--{boundary}\r\ncontent-disposition: form-data; name=\"model\"\r\n\r\njobs-a\r\n"
        );
        for (name, body) in parts {
            b.push_str(&format!(
                "--{boundary}\r\ncontent-disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\r\n{body}\r\n"
            ));
        }
        b.push_str(&format!("--{boundary}--\r\n"));
        let req = Request::builder()
            .method("POST")
            .uri("/v1/files")
            .header("authorization", "Bearer sk-caller")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(axum::body::Body::from(b))
            .unwrap();
        let status = app.oneshot(req).await.unwrap().status();
        let sent = upstream
            .received_requests()
            .await
            .unwrap()
            .first()
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            .unwrap_or_default();
        (status, sent)
    }

    /// Every forwarded `file` part is inspected, not only the last one.
    #[tokio::test]
    async fn every_file_part_is_masked_not_only_the_last() {
        let (status, sent) = upload_with_guardrail(
            r#"{"name":"mask-email","enabled":true,"hook_point":"input","kind":"pii","detectors":[{"type":"email","action":"mask"}]}"#,
            &[("first.jsonl", "FIRST mail alice@example.com"), ("second.jsonl", "SECOND clean")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !sent.contains("alice@example.com"),
            "the first part went out unmasked:\n{sent}"
        );
        assert!(sent.contains("SECOND clean"));
    }

    /// The provider reads JSONL decoded; a rule must see it decoded too. A
    /// JSON escape (`\u0068` = `h`, or the `\uXXXX` Python writes for all
    /// non-ASCII text by default) must not carry a phrase past the check.
    #[tokio::test]
    async fn a_phrase_behind_json_escapes_in_an_uploaded_file_is_refused() {
        let (status, sent) = upload_with_guardrail(
            r#"{"name":"kw","enabled":true,"hook_point":"input","kind":"keyword","patterns":[{"kind":"literal","value":"how to pick a lock"},{"kind":"literal","value":"违禁词"}]}"#,
            &[(
                "b.jsonl",
                r#"{"custom_id":"r1","body":{"messages":[{"role":"user","content":"\u0068ow to pick a lock"}]}}"#,
            )],
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(sent.is_empty(), "nothing may reach the provider");

        // 文件开头有 UTF-8 BOM 时，第一行照样按解码后的内容查。
        let (status, sent) = upload_with_guardrail(
            r#"{"name":"kw","enabled":true,"hook_point":"input","kind":"keyword","patterns":[{"kind":"literal","value":"how to pick a lock"}]}"#,
            &[(
                "b.jsonl",
                "\u{feff}{\"custom_id\":\"r1\",\"body\":{\"messages\":[{\"role\":\"user\",\"content\":\"\\u0068ow to pick a lock\"}]}}",
            )],
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(sent.is_empty(), "nothing may reach the provider");

        let (status, _) = upload_with_guardrail(
            r#"{"name":"kw","enabled":true,"hook_point":"input","kind":"keyword","patterns":[{"kind":"literal","value":"违禁词"}]}"#,
            &[(
                "b.jsonl",
                r#"{"custom_id":"r1","body":{"messages":[{"role":"user","content":"\u8fdd\u7981\u8bcd"}]}}"#,
            )],
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// A line this gateway cannot parse is no way past a rule either: a lone
    /// `\ud800`, a `NaN` or an `Infinity` is refused by this parser and taken
    /// by Python's, which then reads the escaped word the bytes hide. Its
    /// string literals are read as a lenient parser reads them, so the word is
    /// matched whatever the rule's failure policy; text that is no JSON to
    /// anyone, with a backslash in it, passes.
    #[tokio::test]
    async fn an_upload_line_this_gateway_cannot_parse_is_not_read_as_its_bytes() {
        let lines = [
            r#"{"custom_id":"r1","body":{"messages":[{"role":"user","content":"\u0068ow to pick a lock \ud800"}]}}"#,
            r#"{"custom_id":"r1","body":{"x":NaN,"messages":[{"role":"user","content":"\u0068ow to pick a lock"}]}}"#,
            r#"{"custom_id":"r1","body":{"x":Infinity,"messages":[{"role":"user","content":"\u0068ow to pick a lock"}]}}"#,
        ];
        let rule = |fail_open: bool| {
            format!(
                r#"{{"name":"kw","enabled":true,"hook_point":"input","fail_open":{fail_open},"kind":"keyword","patterns":[{{"kind":"literal","value":"how to pick a lock"}}]}}"#
            )
        };
        for line in lines {
            for fail_open in [false, true] {
                let (status, sent) =
                    upload_with_guardrail(&rule(fail_open), &[("b.jsonl", line)]).await;
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{line}");
                assert!(sent.is_empty(), "nothing may reach the provider: {line}");
            }
        }

        for clean in [
            r"see C:\docs\plan for the schedule",
            r"{\bf bold} and {\em italic}",
            r#"["C:\Users\me\data.csv"]"#,
        ] {
            let (status, _) = upload_with_guardrail(&rule(false), &[("notes.txt", clean)]).await;
            assert_eq!(status, StatusCode::OK, "{clean}");
        }
    }

    /// GBK bytes for 你好 inside an otherwise well-formed JSONL line.
    /// `0xC4` opens a two-byte sequence and `0xE3` is not a continuation
    /// byte, so the blob does not decode as UTF-8. It also carries
    /// `custom_id`, the literal [`seed_blocking_input_guardrail`] blocks
    /// on, so one upload exercises both ways a scan of the whole blob
    /// could stop it.
    const BLOB_UPLOAD: &[u8] = b"{\"custom_id\":\"r1\",\"note\":\"\xc4\xe3\xba\xc3\"}";
    /// [`BLOB_UPLOAD`] without what `seed_blocking_input_guardrail` matches.
    const CLEAN_BLOB_UPLOAD: &[u8] = b"{\"id\":\"r1\",\"note\":\"\xc4\xe3\xba\xc3\"}";

    /// A fail-closed input row matching `custom_id`, which every request
    /// fixture below carries. The files surface must forward regardless;
    /// `a_batch_create_is_still_screened` is what proves the literal still
    /// matches, so a pattern that quietly stopped matching cannot turn the
    /// forwarding assertions into tautologies.
    fn seed_blocking_input_guardrail(snap: &AisixSnapshot) {
        let g: aisix_core::Guardrail = serde_json::from_str(
            r#"{"name":"blocks-a-term","enabled":true,"hook_point":"input","fail_open":false,"kind":"keyword","patterns":[{"kind":"literal","value":"custom_id"}]}"#,
        )
        .unwrap();
        crate::seed_env_scoped_guardrail(
            snap,
            aisix_core::resource::ResourceEntry::new("g-in", g, 1),
        );
    }

    /// The response-side counterpart, matching `forbidden-term`, which
    /// every response fixture below carries.
    /// `a_fine_tuning_job_read_is_still_screened` pins that it matches.
    fn seed_blocking_output_guardrail(snap: &AisixSnapshot) {
        let g: aisix_core::Guardrail = serde_json::from_str(
            r#"{"name":"blocks-a-term","enabled":true,"hook_point":"output","fail_open":false,"kind":"keyword","patterns":[{"kind":"literal","value":"forbidden-term"}]}"#,
        )
        .unwrap();
        crate::seed_env_scoped_guardrail(
            snap,
            aisix_core::resource::ResourceEntry::new("g-out", g, 1),
        );
    }

    fn upload_request_with_purpose(
        boundary: &str,
        purpose: Option<&str>,
        file: &[u8],
    ) -> Request<axum::body::Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/files")
            .header("authorization", "Bearer sk-caller")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(axum::body::Body::from(multipart_body_with_purpose(
                boundary,
                Some("jobs-a"),
                purpose,
                file,
            )))
            .unwrap()
    }

    fn files_upstream_mock() -> Mock {
        Mock::given(wm_method("POST"))
            .and(path("/v1/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "file-abc",
                "object": "file",
                "purpose": "batch",
                "filename": "forbidden-term.jsonl"
            })))
    }

    /// An upload whose content matches a block rule is refused before the
    /// provider is contacted, under every class of declared `purpose`: one
    /// whose payload is contractually JSONL, one that legitimately carries
    /// binary, and none at all. The fixture is not valid UTF-8 either, which
    /// must not take it past the scan.
    #[tokio::test]
    async fn a_matching_upload_is_refused_under_every_purpose_class() {
        for purpose in [Some("batch"), Some("assistants"), None] {
            let upstream = MockServer::builder().start().await;
            files_upstream_mock().expect(0).mount(&upstream).await;

            let snap = AisixSnapshot::new();
            snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
            snap.models.insert(model("m-a", "jobs-a", PK_A));
            snap.apikeys.insert(apikey_entry(&["*"]));
            seed_blocking_input_guardrail(&snap);
            let (app, mut rx) = build_app_with_sink(snap);

            let resp = app
                .oneshot(upload_request_with_purpose(
                    "XBOUNDARYX",
                    purpose,
                    BLOB_UPLOAD,
                ))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "purpose {purpose:?}"
            );
            let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["type"], "content_filter", "{body}");
            assert_eq!(
                body["error"]["message"],
                "request blocked by content policy (guardrail 'blocks-a-term')",
            );
            assert!(
                upstream.received_requests().await.unwrap().is_empty(),
                "purpose {purpose:?}: nothing reaches the provider"
            );

            let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("/v1/files must emit a usage event")
                .expect("channel open");
            assert!(ev.guardrail_blocked, "purpose {purpose:?}: {ev:?}");
        }
    }

    /// A clean upload reaches the provider byte for byte under the same
    /// classes, though it is not valid UTF-8: a block rule that matches nothing
    /// has no reason to look further, and nothing went unscreened.
    #[tokio::test]
    async fn an_upload_is_forwarded_under_every_purpose_class() {
        for purpose in [Some("batch"), Some("assistants"), None] {
            let upstream = MockServer::builder().start().await;
            files_upstream_mock().expect(1).mount(&upstream).await;

            let snap = AisixSnapshot::new();
            snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
            snap.models.insert(model("m-a", "jobs-a", PK_A));
            snap.apikeys.insert(apikey_entry(&["*"]));
            seed_blocking_input_guardrail(&snap);
            let (app, mut rx) = build_app_with_sink(snap);

            let resp = app
                .oneshot(upload_request_with_purpose(
                    "XBOUNDARYX",
                    purpose,
                    CLEAN_BLOB_UPLOAD,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "purpose {purpose:?}");

            let received = upstream.received_requests().await.unwrap();
            assert_eq!(received.len(), 1, "purpose {purpose:?}");
            assert!(
                received[0]
                    .body
                    .windows(CLEAN_BLOB_UPLOAD.len())
                    .any(|w| w == CLEAN_BLOB_UPLOAD),
                "purpose {purpose:?}: the original bytes must forward byte-for-byte"
            );

            let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("/v1/files must emit a usage event")
                .expect("channel open");
            assert!(
                ev.guardrail_enforced_hits.is_empty(),
                "purpose {purpose:?}: {ev:?}"
            );
            assert_eq!(
                ev.guardrail_bypassed_reason, "",
                "purpose {purpose:?}: the chain read the file, so nothing was bypassed: {ev:?}"
            );
        }
    }

    /// The upload's response is relayed too. The row matches `input.jsonl`,
    /// which the provider echoes back as the stored file's `filename`.
    #[tokio::test]
    async fn an_upload_response_is_relayed_past_a_blocking_output_row() {
        let upstream = MockServer::builder().start().await;
        files_upstream_mock().expect(1).mount(&upstream).await;

        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.models.insert(model("m-a", "jobs-a", PK_A));
        snap.apikeys.insert(apikey_entry(&["*"]));
        seed_blocking_output_guardrail(&snap);
        let app = build_app(snap);

        let resp = app
            .oneshot(upload_request_with_purpose(
                "XBOUNDARYX",
                Some("batch"),
                br#"{"custom_id":"r1"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["filename"], "forbidden-term.jsonl");
    }

    /// And a download: `GET /v1/files/{id}/content` relays the provider's
    /// bytes verbatim, matching row or not.
    #[tokio::test]
    async fn a_file_download_is_relayed_past_a_blocking_output_row() {
        const STORED: &[u8] = b"{\"custom_id\":\"r1\",\"note\":\"forbidden-term\"}\n";

        let upstream = MockServer::builder().start().await;
        Mock::given(wm_method("GET"))
            .and(path("/v1/files/file-abc/content"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(STORED))
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.models.insert(model("m-a", "jobs-a", PK_A));
        snap.apikeys.insert(apikey_entry(&["*"]));
        seed_blocking_output_guardrail(&snap);
        let app = build_app(snap);

        let req = Request::builder()
            .method("GET")
            .uri("/v1/files/file-abc/content?model=jobs-a")
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        assert_eq!(bytes.as_ref(), STORED);
    }

    /// The scope boundary, request side: `/v1/batches` sends a serialised
    /// JSON request body, which is a different thing from a caller's
    /// uploaded blob and is still screened.
    #[tokio::test]
    async fn a_batch_create_is_still_screened() {
        let upstream = MockServer::builder().start().await;
        Mock::given(wm_method("POST"))
            .and(path("/v1/batches"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "batch_777",
                "object": "batch",
                "status": "validating"
            })))
            .expect(0)
            .mount(&upstream)
            .await;

        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.models.insert(model("m-a", "jobs-a", PK_A));
        snap.apikeys.insert(apikey_entry(&["*"]));
        seed_blocking_input_guardrail(&snap);
        let app = build_app(snap);

        let body = serde_json::json!({
            "input_file_id": encode_routed_id("file-real", "jobs-a", None),
            "endpoint": "/v1/chat/completions",
            "completion_window": "24h",
            "metadata": { "note": "custom_id" }
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/batches")
            .header("authorization", "Bearer sk-caller")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["type"], "content_filter", "{v}");
    }

    /// The scope boundary, response side. Fine-tuning reads share
    /// `forward_simple` with the files reads, so this is what fails if the
    /// files surface is exempted by deleting that scan rather than by
    /// naming the surface it belongs to.
    #[tokio::test]
    async fn a_fine_tuning_job_read_is_still_screened() {
        let upstream = MockServer::builder().start().await;
        Mock::given(wm_method("GET"))
            .and(path("/v1/fine_tuning/jobs/ftjob-9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "ftjob-9",
                "object": "fine_tuning.job",
                "status": "forbidden-term"
            })))
            .mount(&upstream)
            .await;

        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, &upstream.uri()));
        snap.models.insert(model("m-a", "jobs-a", PK_A));
        snap.apikeys.insert(apikey_entry(&["*"]));
        seed_blocking_output_guardrail(&snap);
        let app = build_app(snap);

        let req = Request::builder()
            .method("GET")
            .uri("/v1/fine_tuning/jobs/ftjob-9?model=jobs-a")
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["type"], "content_filter", "{v}");
    }
}
