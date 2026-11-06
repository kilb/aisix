//! Response caching for the endpoints whose bodies the gateway relays
//! rather than models.
//!
//! `/v1/chat/completions` caches a typed [`aisix_gateway::ChatResponse`]
//! because the chat handler owns that shape end to end. Every other
//! cacheable endpoint hands the caller a body it does not model — an
//! Anthropic message, a Responses object, an embedding matrix — so those
//! entries are stored as bytes plus a content type ([`CachedBody`]).
//!
//! One gate object per request, resolved once, so the read path, the write
//! path and the telemetry all agree on which policy matched and under which
//! key. The endpoints that use it differ only in what they put into the key
//! material and how they rebuild a response from a hit.
//!
//! Deliberately EXACT-layer only. The semantic layer answers a request with
//! a *different* request's response when the two are similar enough, which
//! is sound for a chat completion and wrong for an embedding: the whole
//! point of an embedding is that a paraphrase maps to a different vector, so
//! serving a near-match would hand back a vector for text the caller never
//! sent. Rather than enable it where it is safe and skip it where it is not
//! — a distinction no operator could see from the policy — the layer stays
//! chat-only until the policy can express it.

use std::sync::Arc;
use std::time::Duration;

use aisix_cache::{body_fingerprint, Cache, CachedBody};
use aisix_core::models::CacheScope;
use aisix_core::AisixSnapshot;
use aisix_gateway::{ChatResponse, FinishReason};

use crate::auth::AuthenticatedKey;
use crate::chat::CacheStatus;
use crate::state::ProxyState;

/// Ceiling on a buffered response body considered for caching.
///
/// Sized as a backstop rather than a policy: the endpoints that cache here
/// return a message, a Responses object or an embedding matrix, all far
/// below this. A body above it is relayed uncached rather than rejected.
pub(crate) const MAX_CACHED_BODY_BYTES: usize = 32 << 20;

/// Marks a rendered response the gate must not store, for an answer only the
/// site holding the upstream's [`ChatResponse`] can judge. See
/// [`answer_is_storable`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct UnstorableAnswer;

/// Whether an upstream answer may be written to a response cache. One rule for
/// every endpoint that caches an answer:
///
/// - An answer with generated images stays out. Chat would keep megabytes per
///   image. `/v1/messages` and `/v1/responses` discard the images, so what they
///   would keep is the answer without them, which for an image-only answer is
///   an empty message replayed for the policy's whole TTL.
/// - A refusal stays out for the reason a non-200 does: one refusal would
///   become minutes of replayed refusal for a request a later attempt may well
///   answer. The upstream says so with a content-filter finish, or on the
///   OpenAI shape with `message.refusal` beside an ordinary `stop`.
pub(crate) fn answer_is_storable(answer: &ChatResponse) -> bool {
    answer.finish_reason != FinishReason::ContentFilter
        && !answer.messages().any(|m| {
            m.extra.contains_key("images")
                || m.extra
                    .get("refusal")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|refusal| !refusal.is_empty())
        })
}

/// [`answer_is_storable`] for a body relayed from a native upstream, which
/// never became a [`ChatResponse`]. Each field exists in one protocol only, so
/// one check serves both endpoints:
///
/// - `stop_reason: "refusal"` on a Messages answer
///   (<https://docs.anthropic.com/en/api/messages>);
/// - on a Responses object
///   (<https://platform.openai.com/docs/api-reference/responses/object>),
///   `incomplete_details.reason: "content_filter"` or a `refusal` output part;
/// - a Responses object that is not an answer yet or at all: a 200 can carry
///   `status: "failed"`, and `background: true` answers `queued`. Replaying
///   either for the whole TTL would turn one upstream hiccup, or one
///   submission, into every later caller's result.
pub(crate) fn body_is_storable(body: &[u8]) -> bool {
    #[derive(serde::Deserialize)]
    struct Incomplete {
        reason: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Part {
        #[serde(rename = "type")]
        kind: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Item {
        // Only a message item carries an array here.
        content: Option<serde_json::Value>,
    }
    #[derive(serde::Deserialize)]
    struct Answer {
        stop_reason: Option<String>,
        status: Option<String>,
        incomplete_details: Option<Incomplete>,
        output: Option<Vec<Item>>,
    }
    let Ok(answer) = serde_json::from_slice::<Answer>(body) else {
        return true;
    };
    let refused_part = |item: &Item| {
        item.content.clone().is_some_and(|content| {
            serde_json::from_value::<Vec<Part>>(content).is_ok_and(|parts| {
                parts
                    .iter()
                    .any(|part| part.kind.as_deref() == Some("refusal"))
            })
        })
    };
    answer.stop_reason.as_deref() != Some("refusal")
        && answer
            .status
            .as_deref()
            .is_none_or(|status| matches!(status, "completed" | "incomplete"))
        && answer
            .incomplete_details
            .and_then(|d| d.reason)
            .is_none_or(|reason| reason != "content_filter")
        && !answer.output.iter().flatten().any(refused_part)
}

/// What the cache did for one request, as the usage event reports it.
///
/// One struct rather than three loose parameters: the status and the saved
/// counts are only meaningful together, and an emit site that carried the
/// status but dropped the counts would report a hit that saved nothing.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CacheTelemetry {
    pub(crate) status: CacheStatus,
    /// Upstream tokens the hit avoided spending. Zero unless `status` is
    /// `Hit`.
    pub(crate) saved_input_tokens: u32,
    pub(crate) saved_output_tokens: u32,
}

impl CacheTelemetry {
    pub(crate) fn hit(body: &CachedBody) -> Self {
        Self {
            status: CacheStatus::Hit,
            saved_input_tokens: body.prompt_tokens,
            saved_output_tokens: body.completion_tokens,
        }
    }

    pub(crate) fn of(status: CacheStatus) -> Self {
        Self {
            status,
            ..Self::default()
        }
    }

    /// This request's upstream price, which a cache hit does not have.
    ///
    /// 命中缓存的请求没有打上游，网关没为它付过一分钱 —— 所以它的花费是 0，
    /// 而不是「把重放的 token 按价目表再算一遍」。`/v1/chat/completions` 一直
    /// 是这个契约（命中分支上直接写 `cost_usd: 0.0`，并把重放的 token 记进
    /// `cache_hit_saved_*` 供控制面算省了多少）。
    ///
    /// 定价走这里而不是让每个端点自己判一次：带缓存闸的端点现在有三个
    /// （`/v1/messages`、`/v1/responses`、`/v1/embeddings`），条件写三遍就是
    /// 三份各自会漂的副本 —— 这个 bug 本来就是「chat 修了、另两个没修」漏出
    /// 来的，补的时候又漏了第三个。拿得到 `CacheTelemetry` 的调用点用这个，
    /// 拿不到的（没有缓存闸的那几个端点）继续直接用 `request_cost_usd`。
    ///
    /// 命中时 token 计数照报：它们是「这次省下了多少」的依据，只有价钱是 0。
    pub(crate) fn cost_usd(
        &self,
        snap: &aisix_core::AisixSnapshot,
        model_id: &str,
        input: aisix_core::InputTokens,
        output_tokens: u64,
    ) -> f64 {
        if matches!(self.status, CacheStatus::Hit) {
            return 0.0;
        }
        crate::usage_attr::request_cost_usd(snap, model_id, input, output_tokens)
    }

    /// `cache_hit_layer` for the usage event — this gate is exact-only.
    pub(crate) fn layer(&self) -> String {
        if matches!(self.status, CacheStatus::Hit) {
            "exact".to_string()
        } else {
            String::new()
        }
    }
}

/// Who may share a cache entry under a policy's `scope`, as the value that
/// enters every cache key: the chat exact key and semantic partition, and
/// the fingerprint [`BodyCache`] builds for every other endpoint. One
/// function, so no endpoint can draw the sharing boundary differently.
///
/// It keys on the caller alone, never on a model, so every model kind is
/// scoped the same way.
///
/// `None` is the one value that shares an entry with every caller, and only
/// `env` returns it, so a scope that yields some other value can never widen
/// to environment-wide by accident.
///
/// - `api_key`: the key's id, bare. That is the value every entry stored
///   under this scope has always been keyed by, so those entries stay
///   reachable.
/// - `env`: `None`, so every caller shares, as it always has.
/// - `team`: `team:<team_id>`, shared by the keys of one team. A key with no
///   team, or an empty `team_id`, gets `key:<id>` and shares with no one: a
///   key nobody assigned to a tenant must not land in some tenant's entries.
///   The two prefixes keep a team's value from ever equalling a key's.
pub(crate) fn cache_scope_value(scope: CacheScope, auth: &AuthenticatedKey) -> Option<String> {
    match scope {
        CacheScope::ApiKey => Some(auth.entry.id.clone()),
        CacheScope::Env => None,
        CacheScope::Team => Some(
            match auth
                .key()
                .team_id
                .as_deref()
                .filter(|team| !team.is_empty())
            {
                Some(team) => format!("team:{team}"),
                None => format!("key:{}", auth.entry.id),
            },
        ),
    }
}

/// A scope value as a part of the body fingerprint. Entries every caller
/// shares have always been stored under an empty part, so only `None` is
/// spelled that way. An empty `Some` has no spelling of its own in that
/// key, so it gets `None` back and the request goes uncached instead of
/// landing in the shared entries.
fn body_scope_part(scope_value: Option<&str>) -> Option<&str> {
    match scope_value {
        None => Some(""),
        Some("") => None,
        Some(value) => Some(value),
    }
}

/// A resolved cache gate for one request on one endpoint.
pub(crate) struct BodyCache {
    cache: Arc<dyn Cache>,
    key: String,
    ttl: Duration,
    policy_name: String,
    /// `Cache-Control: no-cache` — skip the read, still refresh the entry.
    no_cache: bool,
    /// `Cache-Control: no-store` — read allowed, writes suppressed.
    no_store: bool,
}

impl BodyCache {
    /// Resolve the gate for this request, or `None` when no enabled policy
    /// covers it, the policy's backend is unavailable, or the caller's scope
    /// value cannot be keyed (see [`body_scope_part`]).
    ///
    /// `key_material` is the endpoint's canonical request identity. Callers
    /// pass the fields that change the upstream answer; `serde_json`'s maps
    /// are sorted, so a serialized request body is already independent of
    /// the caller's JSON key order. A closure because it serializes the whole
    /// request, and most deployments have no policy to compute it for.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn resolve(
        state: &ProxyState,
        snapshot: &AisixSnapshot,
        auth: &AuthenticatedKey,
        client: &crate::client_ip::ClientContext,
        endpoint: &str,
        model_name: &str,
        model: &aisix_core::Model,
        key_material: impl FnOnce() -> String,
    ) -> Option<Self> {
        // Same match rule as the chat gate: the first enabled policy whose
        // `applies_to` accepts (model, caller). A policy has no endpoint
        // dimension, so one written for a model covers that model wherever
        // it is addressed.
        let policies = snapshot.cache_policies.entries();
        let entry = policies.iter().find(|entry| {
            entry.value.enabled
                && entry
                    .value
                    .parsed_applies_to(snapshot)
                    .matches(model_name, &auth.entry.id)
        })?;
        let cache = state
            .cache
            .as_ref()?
            .for_policy_backend(entry.value.backend, &entry.id, &entry.value.name)?
            .clone();
        let scope_value = cache_scope_value(entry.value.scope, auth);
        let scope_part = body_scope_part(scope_value.as_deref())?;
        let generation = entry.value.purge_generation.to_string();
        let key_material = key_material();
        let vary_tags = crate::routing::cache_vary_tags(model, &client.routing_tags);
        let mut parts = vec![
            endpoint,
            model_name,
            &key_material,
            &entry.id,
            &generation,
            scope_part,
        ];
        // Only when present, so every entry of a model routing tags cannot
        // steer keeps its fingerprint.
        parts.extend(vary_tags.as_deref());
        let key = body_fingerprint(&parts);
        let directives = crate::chat::cache_control_directives_of(Some(client.headers.as_ref()));
        Some(Self {
            cache,
            key,
            ttl: Duration::from_secs(u64::from(entry.value.ttl_seconds)),
            policy_name: entry.value.name.clone(),
            no_cache: directives.0,
            no_store: directives.1,
        })
    }

    /// Status to report when the gate produced no hit.
    pub(crate) fn miss_status(&self) -> CacheStatus {
        if self.no_cache {
            CacheStatus::Bypass
        } else {
            CacheStatus::Miss
        }
    }

    /// Read path. `None` on a miss, on a caller bypass, or on a backend
    /// failure — a cache that cannot answer is a miss, never an error the
    /// caller sees.
    pub(crate) async fn lookup(&self) -> Option<CachedBody> {
        if self.no_cache {
            return None;
        }
        match self.cache.get_body(&self.key).await {
            Ok(hit @ Some(_)) => hit,
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(
                    target: "aisix::cache",
                    policy_name = %self.policy_name,
                    error = %err,
                    "cache lookup failed; request proceeds uncached",
                );
                None
            }
        }
    }

    /// Write path, called only for a response the caller actually received.
    pub(crate) async fn store(&self, body: CachedBody) {
        if self.no_store {
            return;
        }
        if let Err(err) = self
            .cache
            .put_body_with_ttl(&self.key, body, self.ttl)
            .await
        {
            tracing::warn!(
                target: "aisix::cache",
                policy_name = %self.policy_name,
                error = %err,
                "cache write failed; the entry is simply absent next time",
            );
        }
    }

    /// Record the request's outcome on `aisix_cache_requests_total`, the
    /// same series the chat gate feeds, so one dashboard covers every
    /// endpoint.
    ///
    /// The label comes from [`CacheStatus::metric_outcome`], never from
    /// `as_str()`: that is the control plane wire value, and feeding it here
    /// emitted `outcome="hit"` where chat emits `hit_exact` — one series,
    /// two vocabularies, so a dashboard covered chat and silently showed
    /// nothing for these endpoints.
    pub(crate) fn record(&self, state: &ProxyState, status: CacheStatus) {
        // These endpoints run the exact layer only; `None` says so.
        if let Some(outcome) = status.metric_outcome(None) {
            state.metrics.record_cache_event(&self.policy_name, outcome);
        }
    }
}

/// Attach the cache headers the chat surface already publishes, so a client
/// can tell a replayed answer from a fresh one on any endpoint.
///
/// In platform mode they are debug headers, as they are on chat: sent only
/// with `proxy.expose_debug_headers` on. Under a scope shared across keys, a
/// hit tells a caller that another key already asked the same thing.
pub(crate) fn apply_cache_headers(
    state: &ProxyState,
    response: &mut axum::response::Response,
    status: CacheStatus,
) {
    if matches!(status, CacheStatus::Disabled)
        || (state.platform().is_some() && !state.expose_debug_headers)
    {
        return;
    }
    if let Ok(value) = axum::http::HeaderValue::from_str(status.as_str()) {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(crate::chat::CACHE_HEADER),
            value,
        );
    }
    if matches!(status, CacheStatus::Hit) {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(crate::chat::CACHE_LAYER_HEADER),
            axum::http::HeaderValue::from_static("exact"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aisix_gateway::{ChatMessage, UsageStats};

    fn answer(finish_reason: FinishReason) -> ChatResponse {
        ChatResponse {
            logprobs: None,
            more_choices: Vec::new(),
            id: "id".to_string(),
            model: "m".to_string(),
            message: ChatMessage::assistant("text".to_string()),
            finish_reason,
            usage: UsageStats::new(0, 0),
            service_tier: None,
        }
    }

    #[test]
    fn an_answer_with_images_or_a_refusal_is_not_stored() {
        assert!(answer_is_storable(&answer(FinishReason::Stop)));
        assert!(answer_is_storable(&answer(FinishReason::Length)));
        assert!(!answer_is_storable(&answer(FinishReason::ContentFilter)));

        let mut with_images = answer(FinishReason::Stop);
        with_images
            .message
            .extra
            .insert("images".to_string(), serde_json::json!([]));
        assert!(!answer_is_storable(&with_images));

        // The OpenAI shape reports a refusal beside an ordinary `stop`.
        let mut refused = answer(FinishReason::Stop);
        refused
            .message
            .extra
            .insert("refusal".to_string(), serde_json::json!("I can't help."));
        assert!(!answer_is_storable(&refused));
        let mut no_refusal = answer(FinishReason::Stop);
        no_refusal
            .message
            .extra
            .insert("refusal".to_string(), serde_json::Value::Null);
        assert!(answer_is_storable(&no_refusal));
    }

    /// The key material serializes the whole request, and most deployments have
    /// no cache policy to compute it for.
    #[test]
    fn with_no_policy_the_key_material_is_never_computed() {
        let snap = AisixSnapshot::new();
        let state = crate::ProxyState::new(
            aisix_core::snapshot::SnapshotHandle::new(snap.clone()),
            Arc::new(aisix_gateway::Hub::new()),
            &serde_json::from_value::<aisix_core::ProxyConfig>(serde_json::json!({
                "addr": "127.0.0.1:0"
            }))
            .unwrap(),
        );
        let key: aisix_core::ApiKey =
            serde_json::from_value(serde_json::json!({"key_hash": "h"})).unwrap();
        let auth = AuthenticatedKey::new(
            &snap,
            Arc::new(aisix_core::resource::ResourceEntry::new("k", key, 1)),
            None,
            false,
        );
        let model: aisix_core::Model = serde_json::from_value(serde_json::json!({
            "display_name": "m",
            "provider": "openai",
            "model_name": "gpt-4o",
            "provider_key_id": "pk",
        }))
        .unwrap();
        let gate = BodyCache::resolve(
            &state,
            &snap,
            &auth,
            &crate::client_ip::ClientContext::default(),
            "/v1/embeddings",
            "m",
            &model,
            || panic!("computed the key for a request no policy covers"),
        );
        assert!(gate.is_none());
    }

    #[test]
    fn a_relayed_body_that_is_not_an_answer_is_not_stored() {
        for not_stored in [
            r#"{"type":"message","stop_reason":"refusal","content":[]}"#,
            r#"{"status":"incomplete","incomplete_details":{"reason":"content_filter"}}"#,
            r#"{"status":"failed","error":{"code":"server_error"}}"#,
            r#"{"status":"queued"}"#,
            r#"{"status":"in_progress"}"#,
            r#"{"status":"cancelled"}"#,
            r#"{"status":"completed","output":[{"type":"message","content":[{"type":"refusal","refusal":"no"}]}]}"#,
        ] {
            assert!(!body_is_storable(not_stored.as_bytes()), "{not_stored}");
        }
        for stored in [
            r#"{"type":"message","stop_reason":"end_turn","content":[]}"#,
            r#"{"status":"completed","incomplete_details":null}"#,
            r#"{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}"#,
            r#"{"status":"completed","output":[{"type":"reasoning","content":null},{"type":"message","content":[{"type":"output_text","text":"hi"}]}]}"#,
            r#"{"data":[{"embedding":[0.1]}]}"#,
            "not json",
        ] {
            assert!(body_is_storable(stored.as_bytes()), "{stored}");
        }
    }

    /// The three cache keys one request is stored under: the chat exact
    /// key, the chat semantic partition, and the body fingerprint the
    /// other endpoints use.
    #[derive(Debug, PartialEq)]
    struct Keys {
        exact: String,
        partition: String,
        body: String,
    }

    /// The same request, under a policy of `scope`, from the key `key_id`
    /// carrying `team_id`.
    fn key_for(scope: CacheScope, team_id: Option<&str>, key_id: &str) -> Keys {
        use aisix_core::resource::ResourceEntry;
        let snap = AisixSnapshot::new();
        let mut policy: aisix_core::CachePolicy =
            serde_json::from_value(serde_json::json!({"name": "p"})).unwrap();
        policy.scope = scope;
        snap.cache_policies
            .insert(ResourceEntry::new("policy-1", policy, 1));
        let state = crate::ProxyState::new(
            aisix_core::snapshot::SnapshotHandle::new(snap.clone()),
            Arc::new(aisix_gateway::Hub::new()),
            &serde_json::from_value::<aisix_core::ProxyConfig>(serde_json::json!({
                "addr": "127.0.0.1:0"
            }))
            .unwrap(),
        );
        let key: aisix_core::ApiKey =
            serde_json::from_value(serde_json::json!({"key_hash": "h", "team_id": team_id}))
                .unwrap();
        let auth = AuthenticatedKey::new(
            &snap,
            Arc::new(ResourceEntry::new(key_id, key, 1)),
            None,
            false,
        );
        let model: aisix_core::Model = serde_json::from_value(serde_json::json!({
            "display_name": "m",
            "provider": "openai",
            "model_name": "gpt-4o",
            "provider_key_id": "pk",
        }))
        .unwrap();
        let gate = BodyCache::resolve(
            &state,
            &snap,
            &auth,
            &crate::client_ip::ClientContext::default(),
            "/v1/embeddings",
            "m",
            &model,
            || "{\"input\":\"hi\"}".to_string(),
        )
        .expect("the policy covers the request");
        let req = aisix_gateway::ChatFormat::new("m", vec![ChatMessage::user("hi")]);
        let chat = aisix_cache::CacheKey::from_request(&req).with_scope(
            "policy-1",
            0,
            cache_scope_value(scope, &auth).as_deref(),
        );
        Keys {
            exact: chat.fingerprint(),
            partition: chat.scope_fingerprint(),
            body: gate.key,
        }
    }

    /// Every one of the three keys differs: one shared key family is
    /// enough to serve one caller's answer to the other.
    fn assert_isolated(a: &Keys, b: &Keys) {
        assert_ne!(a.exact, b.exact, "exact keys shared: {a:?} {b:?}");
        assert_ne!(
            a.partition, b.partition,
            "semantic partitions shared: {a:?} {b:?}"
        );
        assert_ne!(a.body, b.body, "body keys shared: {a:?} {b:?}");
    }

    /// Computed with the same inputs on the build before `team` existed, so
    /// entries already stored under `api_key` and `env` stay reachable.
    const LEGACY_API_KEY_FINGERPRINT: (&str, &str, &str) = (
        "c76ef486be681e1e49c7720465407255",
        "e0838d7341a70de5bbf2fa14c4fb68e5",
        "dc3096e47cbc7cd79cc54ba3cf79bc7e",
    );
    const LEGACY_ENV_FINGERPRINT: (&str, &str, &str) = (
        "15c70090121c892939eb2f910f8e0e4c",
        "4a3f897c1b09e90ba260f062e3657e9b",
        "87cd351069ced1c945daf7b4f40e916a",
    );

    fn legacy(fp: (&str, &str, &str)) -> Keys {
        Keys {
            exact: fp.0.to_string(),
            partition: fp.1.to_string(),
            body: fp.2.to_string(),
        }
    }

    #[test]
    fn two_teams_never_share_a_team_scoped_entry() {
        assert_isolated(
            &key_for(CacheScope::Team, Some("t1"), "k1"),
            &key_for(CacheScope::Team, Some("t2"), "k2"),
        );
    }

    #[test]
    fn keys_of_one_team_share_a_team_scoped_entry() {
        assert_eq!(
            key_for(CacheScope::Team, Some("t1"), "k1"),
            key_for(CacheScope::Team, Some("t1"), "k2"),
        );
    }

    #[test]
    fn a_key_without_a_team_is_isolated_under_team_scope() {
        assert_isolated(
            &key_for(CacheScope::Team, None, "k1"),
            &key_for(CacheScope::Team, None, "k2"),
        );
        // An empty `team_id` names no team either.
        assert_isolated(
            &key_for(CacheScope::Team, Some(""), "k1"),
            &key_for(CacheScope::Team, Some(""), "k2"),
        );
        // A key with no team never lands in a team's entries, even one whose
        // id is spelled like the key's.
        assert_isolated(
            &key_for(CacheScope::Team, None, "k1"),
            &key_for(CacheScope::Team, Some("k1"), "k2"),
        );
    }

    /// `None` is the only value that shares with every caller, and only `env`
    /// produces it: every other scope names a non-empty boundary, with or
    /// without a team on the key.
    #[test]
    fn only_the_env_scope_shares_with_every_caller() {
        use aisix_core::resource::ResourceEntry;
        for team_id in [None, Some(""), Some("t1")] {
            let snap = AisixSnapshot::new();
            let key: aisix_core::ApiKey =
                serde_json::from_value(serde_json::json!({"key_hash": "h", "team_id": team_id}))
                    .unwrap();
            let auth = AuthenticatedKey::new(
                &snap,
                Arc::new(ResourceEntry::new("k1", key, 1)),
                None,
                false,
            );
            assert_eq!(cache_scope_value(CacheScope::Env, &auth), None);
            for scope in [CacheScope::ApiKey, CacheScope::Team] {
                let value = cache_scope_value(scope, &auth);
                assert!(
                    value.as_deref().is_some_and(|v| !v.is_empty()),
                    "{scope:?} with team {team_id:?} gave {value:?}"
                );
            }
        }
    }

    /// An empty value is not a way into the entries every caller shares, on
    /// either key family: the chat key hashes it as its own bucket, and the
    /// body key, which spells the shared entries as an empty part, refuses it.
    #[test]
    fn an_empty_scope_value_never_lands_in_the_shared_entries() {
        let req = aisix_gateway::ChatFormat::new("m", vec![ChatMessage::user("hi")]);
        let empty = aisix_cache::CacheKey::from_request(&req).with_scope("policy-1", 0, Some(""));
        let shared = legacy(LEGACY_ENV_FINGERPRINT);
        assert_ne!(empty.fingerprint(), shared.exact);
        assert_ne!(empty.scope_fingerprint(), shared.partition);

        assert_eq!(body_scope_part(None), Some(""));
        assert_eq!(body_scope_part(Some("")), None);
        assert_eq!(body_scope_part(Some("k1")), Some("k1"));
    }

    #[test]
    fn existing_api_key_and_env_keys_are_unchanged() {
        assert_eq!(
            key_for(CacheScope::ApiKey, Some("t1"), "k1"),
            legacy(LEGACY_API_KEY_FINGERPRINT)
        );
        assert_eq!(
            key_for(CacheScope::Env, Some("t1"), "k1"),
            legacy(LEGACY_ENV_FINGERPRINT)
        );
    }

    /// The semantic layer only compares a request against entries in its
    /// own partition, so a near match from another tenant must not even be
    /// a candidate, while a teammate's is.
    #[tokio::test]
    async fn semantic_partitions_are_per_team_under_team_scope() {
        use aisix_cache::{MemorySemanticCache, SemanticCacheStore};
        let partition =
            |team: Option<&str>, key: &str| key_for(CacheScope::Team, team, key).partition;
        let store = MemorySemanticCache::new();
        store
            .store(
                "policy-1",
                0,
                &partition(Some("t1"), "k1"),
                "exact",
                vec![1.0, 0.0],
                answer(FinishReason::Stop),
                Duration::from_secs(60),
                100,
            )
            .await
            .unwrap();
        let lookup = |fp: String| {
            let store = &store;
            async move {
                store
                    .lookup("policy-1", 0, &fp, &[1.0, 0.0], 0.5)
                    .await
                    .unwrap()
                    .is_some()
            }
        };
        assert!(
            lookup(partition(Some("t1"), "k2")).await,
            "a teammate's entry is a candidate"
        );
        assert!(
            !lookup(partition(Some("t2"), "k3")).await,
            "another team's entry is not"
        );
        assert!(
            !lookup(partition(None, "k4")).await,
            "a key with no team sees no team's entries"
        );
    }

    /// A gateway in platform mode, or not, with the debug headers on or off.
    fn state(platform: bool, expose_debug_headers: bool) -> crate::ProxyState {
        let state = crate::ProxyState::new(
            aisix_core::snapshot::SnapshotHandle::new(AisixSnapshot::new()),
            Arc::new(aisix_gateway::Hub::new()),
            &serde_json::from_value::<aisix_core::ProxyConfig>(serde_json::json!({
                "addr": "127.0.0.1:0",
                "expose_debug_headers": expose_debug_headers,
            }))
            .unwrap(),
        );
        if platform {
            state.with_platform(crate::platform::PlatformSettings::from_secret("s"))
        } else {
            state
        }
    }

    fn platform_state(expose_debug_headers: bool) -> crate::ProxyState {
        state(true, expose_debug_headers)
    }

    /// An answer the cache served, as `state` sends it.
    fn cached_response(state: crate::ProxyState) -> axum::response::Response {
        let mut resp = axum::response::Response::new(axum::body::Body::empty());
        apply_cache_headers(&state, &mut resp, CacheStatus::Hit);
        resp
    }

    #[test]
    fn platform_mode_sends_no_cache_headers_without_debug_headers() {
        let resp = cached_response(platform_state(false));
        assert!(resp.headers().get("x-aisix-cache").is_none());
        assert!(resp.headers().get("x-aisix-cache-layer").is_none());
        let mut miss = axum::response::Response::new(axum::body::Body::empty());
        apply_cache_headers(&platform_state(false), &mut miss, CacheStatus::Miss);
        assert!(miss.headers().is_empty(), "{:?}", miss.headers());
    }

    #[test]
    fn platform_mode_sends_cache_headers_with_debug_headers() {
        let resp = cached_response(platform_state(true));
        assert_eq!(resp.headers()["x-aisix-cache"], "hit");
        assert_eq!(resp.headers()["x-aisix-cache-layer"], "exact");
    }

    /// Outside platform mode these endpoints have always said whether the
    /// cache answered, debug headers or not.
    #[test]
    fn outside_platform_mode_cache_headers_are_sent_whatever_the_debug_headers() {
        for debug in [false, true] {
            let resp = cached_response(state(false, debug));
            assert_eq!(resp.headers()["x-aisix-cache"], "hit", "debug {debug}");
            assert_eq!(
                resp.headers()["x-aisix-cache-layer"],
                "exact",
                "debug {debug}"
            );
        }
    }
}
