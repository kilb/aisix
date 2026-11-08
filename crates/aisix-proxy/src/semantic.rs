//! Semantic-routing runtime: embed the request, score it against each
//! route's cached example embeddings, and resolve a single direct-model
//! target (or the router's `default`).
//!
//! [`resolve`] produces a one-element `attempt_models` list that the
//! existing chat dispatch loop then drives exactly like a routing target —
//! so semantic routing reuses all of the streaming / failover / telemetry
//! machinery and only adds the "which target" decision on top.
//!
//! A router with a `classifier` block makes the same decision a different
//! way: one call to a hosted decision model ([`classify`]) picks a route by
//! its description. Everything after the pick — the member gates, the
//! failure policy, the served-route attribution — is shared.
//!
//! The scoring core ([`decide`], [`failure_target`]) and the
//! example-vector cache ([`SemanticVectorCache`]) are pure and unit-tested
//! in isolation; the async embedding call lives in [`resolve`].

use std::borrow::Cow;
use std::sync::Arc;

use dashmap::DashMap;

use aisix_core::models::{
    resolve_model_ref, EmbeddingFailureMode, OnEmbeddingFailure, Semantic, SemanticClassifier,
    SemanticRoute, TYPESAFE_PROVIDER,
};
use aisix_core::resource::ResourceEntry;
use aisix_core::{AisixSnapshot, Model};
use aisix_gateway::{EmbeddingRequest, EmbeddingUsageSource, EmbeddingVector};
use aisix_obs::{
    GatewayEmbeddingCall, GatewayEmbeddingOutcome, GatewayEmbeddingPurpose,
    GatewayEmbeddingUsageSource, SemanticAccessLog, SemanticFallback,
};

use crate::error::ProxyError;
use crate::routing::AttemptModel;
use crate::state::ProxyState;

// Scoring primitives are shared with the `kind: "semantic"` guardrail,
// which scores the same way against its example prototypes — one
// implementation in `aisix-core` so the two cannot drift on the
// degenerate cases (length mismatch, zero magnitude, empty prototype
// set).
pub(crate) use aisix_core::best_similarity;

/// Outcome of scoring a request against a semantic router.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RouteDecision {
    /// Index into `Semantic::routes` of the winning route, or `None` when
    /// no route cleared its threshold (the caller falls back to `default`).
    pub winner: Option<usize>,
    /// Per-route aggregated score, aligned with `Semantic::routes`.
    pub scores: Vec<f32>,
}

/// Score `request_vec` against each route's example vectors and pick the
/// highest-scoring route that clears its (per-route or router-level)
/// threshold. `max` aggregation: a route's score is its single
/// best-matching example. `route_example_vecs[i]` are the cached vectors
/// for `semantic.routes[i]`.
pub(crate) fn decide(
    semantic: &Semantic,
    request_vec: &[f32],
    route_example_vecs: &[Vec<Arc<Vec<f32>>>],
) -> RouteDecision {
    let mut scores = Vec::with_capacity(semantic.routes.len());
    for ex_vecs in route_example_vecs {
        // A route with no cached example vectors scores 0.0, not
        // `-inf`: it must lose to every real route without poisoning the
        // comparison.
        scores.push(
            best_similarity(request_vec, ex_vecs.iter().map(|v| v.as_slice())).unwrap_or(0.0),
        );
    }
    let mut winner: Option<usize> = None;
    let mut best = f32::NEG_INFINITY;
    for (i, route) in semantic.routes.iter().enumerate() {
        if scores[i] >= semantic.route_threshold(route) && scores[i] > best {
            best = scores[i];
            winner = Some(i);
        }
    }
    RouteDecision { winner, scores }
}

/// Why a semantic router could not make its decision: the closed vocabulary
/// [`ProxyError::SemanticDecisionFailed`] carries to the caller and the
/// access log. Free text about the failure goes only to the warn line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionFailure {
    ClassifierTimeout,
    /// The classifier answered a non-2xx status, or the call failed in
    /// transport.
    ClassifierUpstream,
    /// The classifier answered 2xx with no usable choice.
    ClassifierInvalidResponse,
    /// The classifier's provider key is missing or not a `typesafe` one.
    ClassifierUnavailable,
    EmbeddingTimeout,
    /// The embedding upstream answered an error, or the call failed in
    /// transport or decoding.
    EmbeddingUpstream,
    /// The embedding model is missing, misconfigured, or returned vectors
    /// that do not fit it.
    EmbeddingUnavailable,
}

impl DecisionFailure {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ClassifierTimeout => "classifier_timeout",
            Self::ClassifierUpstream => "classifier_upstream",
            Self::ClassifierInvalidResponse => "classifier_invalid_response",
            Self::ClassifierUnavailable => "classifier_unavailable",
            Self::EmbeddingTimeout => "embedding_timeout",
            Self::EmbeddingUpstream => "embedding_upstream",
            Self::EmbeddingUnavailable => "embedding_unavailable",
        }
    }

    fn of_classifier_transport(err: &reqwest::Error) -> (Self, String) {
        let failure = if err.is_timeout() {
            Self::ClassifierTimeout
        } else {
            Self::ClassifierUpstream
        };
        (failure, aisix_gateway::transport_error_message(err))
    }

    fn of_embedding_error(err: &EmbedTextsError) -> Self {
        use aisix_gateway::BridgeError;
        match err {
            EmbedTextsError::Dispatch(ProxyError::Bridge(BridgeError::Timeout { .. })) => {
                Self::EmbeddingTimeout
            }
            EmbedTextsError::Dispatch(ProxyError::Bridge(
                BridgeError::UpstreamStatus { .. }
                | BridgeError::UpstreamInBand { .. }
                | BridgeError::UpstreamDecode(_)
                | BridgeError::UpstreamBodyUnread { .. }
                | BridgeError::TokenExchangeDecode(_)
                | BridgeError::Transport(_)
                | BridgeError::StreamAborted,
            )) => Self::EmbeddingUpstream,
            EmbedTextsError::Dispatch(_) | EmbedTextsError::UnusableResponse(_) => {
                Self::EmbeddingUnavailable
            }
        }
    }
}

impl std::fmt::Display for DecisionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Direct-model alias to dispatch to when the router cannot decide — the
/// embedding call failed (`on_embedding_failure`), or the classifier call
/// did (`classifier.on_failure`). `None` means the policy is `fail` — the
/// caller returns `503`.
///
/// Each of the two aliases it can return is a model reference in its own
/// right, so both are resolved against the snapshot: an id spelling follows
/// a rename, and an id that resolves to nothing comes back as itself and
/// dispatches nowhere, exactly as a dangling alias does.
pub(crate) fn failure_target<'a>(
    snapshot: &AisixSnapshot,
    semantic: &'a Semantic,
) -> Option<Cow<'a, str>> {
    let policy = match &semantic.classifier {
        Some(classifier) => &classifier.on_failure,
        None => &semantic.on_embedding_failure,
    };
    match policy {
        OnEmbeddingFailure::Mode(EmbeddingFailureMode::Default) => {
            Some(semantic.default_ref(snapshot))
        }
        OnEmbeddingFailure::Mode(EmbeddingFailureMode::Fail) => None,
        OnEmbeddingFailure::Target { target, target_id } => {
            Some(resolve_model_ref(snapshot, target, target_id.as_deref()))
        }
    }
}

/// Per-instance cache of route example-utterance embeddings, populated
/// lazily on the first request that needs them and reused across requests
/// so a steady-state request's logical embedding batch contains only the
/// prompt. Keyed by `(vector_scope, dimensions, example_text)` — changing
/// the embedding model, the upstream it names or its dimensions
/// auto-invalidates, since vectors from another embedding space compare as
/// noise and nothing would report it.
#[derive(Debug, Default)]
pub struct SemanticVectorCache {
    vectors: DashMap<(String, u32, String), Arc<Vec<f32>>>,
}

impl SemanticVectorCache {
    /// What a cached vector is only valid for: the row, and the upstream
    /// model and account that row embeds with. A row can be repointed at
    /// another model of the same dimensions in place.
    pub(crate) fn vector_scope(embedding_model_id: &str, model: &Model) -> String {
        format!(
            "{embedding_model_id}\0{}\0{}",
            model.model_name.as_deref().unwrap_or_default(),
            model.provider_key_id.as_deref().unwrap_or_default(),
        )
    }

    /// Embeddings cached; nothing is ever evicted.
    pub fn entry_count(&self) -> u64 {
        self.vectors.len() as u64
    }

    pub(crate) fn get(
        &self,
        vector_scope: &str,
        dimensions: u32,
        text: &str,
    ) -> Option<Arc<Vec<f32>>> {
        self.vectors
            .get(&(vector_scope.to_string(), dimensions, text.to_string()))
            .map(|e| e.clone())
    }

    pub(crate) fn insert(
        &self,
        vector_scope: &str,
        dimensions: u32,
        text: &str,
        vec: Arc<Vec<f32>>,
    ) {
        self.vectors.insert(
            (vector_scope.to_string(), dimensions, text.to_string()),
            vec,
        );
    }
}

/// What a router's decision step produced, before the member gates run.
struct Pick {
    /// Index into `Semantic::routes` of the chosen route; `None` sends the
    /// request to `default`.
    winner: Option<usize>,
    /// The deciding score: the winning (or, with no winner, the best)
    /// route's cosine similarity, or the classifier's confidence.
    score: Option<f64>,
    /// Why there is no winner.
    miss: Option<SemanticFallback>,
    /// 这次决策本身花的钱（micro-USD）。只有嵌入决策有：分类器不是一条
    /// Model，既不按模型计量也不定价。
    embed_spend: u64,
}

/// Resolve a semantic router to a single direct-model attempt + the name
/// of the route that matched (`None` when the request fell through to
/// `default`). The returned `Vec<AttemptModel>` always has exactly one
/// element; the chat dispatch loop drives it like any routing target.
///
/// How it decided is published for the request's access-log line
/// ([`crate::attribution::note_semantic_decision`]).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    router_entry: &ResourceEntry<Model>,
    auth: &crate::auth::AuthenticatedKey,
    prompt: &str,
    source_ip: &str,
    request_id: &str,
    // 分类阶段已计过请求数、只因路由名才匹配上的策略桶，交给目标阶段去重
    // （见 `quota::ParentCounted`）。被拒或降级时已记下的照样有效。
    parent_counted: &mut Vec<String>,
) -> Result<(Vec<AttemptModel>, Option<String>, u64), ProxyError> {
    let semantic = router_entry
        .value
        .semantic
        .as_ref()
        .expect("resolve called on a non-semantic model");
    let router = &router_entry.value;
    // The router's targets bypass `routing::resolve_attempt_models`, so the
    // key's provider groups are applied here, to every candidate it can pick.
    let groups = auth.key().allowed_provider_groups.as_slice();

    // Every alias this function dispatches to is read through the model
    // reference helpers: a router that names its targets by id keeps
    // working across a rename of any of them, and an id that resolves to
    // nothing behaves as the dangling alias it stands in for.
    let default_target = semantic.default_ref(snapshot);

    // No user text to classify (e.g. a system-only or tool-only request):
    // route to `default` without a decision call rather than deciding on
    // an empty string, which could spuriously match a route.
    if prompt.trim().is_empty() {
        note_decision(None, None, Some(SemanticFallback::EmptyPrompt));
        let (attempt, _) = select_eligible(
            state,
            snapshot,
            router,
            source_ip,
            groups,
            &default_target,
            None,
        )?;
        // 没有可分类的文本 → 不发嵌入调用 → 这次没有额外花费。
        return Ok((vec![attempt], None, 0));
    }

    let pick = match &semantic.classifier {
        Some(classifier) => match classify(snapshot, &semantic.routes, classifier, prompt).await {
            Ok((picked, confidence)) => {
                let confident = confidence >= classifier.min_confidence;
                Ok(Pick {
                    winner: confident.then_some(picked),
                    score: Some(confidence),
                    miss: (!confident).then_some(SemanticFallback::LowConfidence),
                    embed_spend: 0,
                })
            }
            Err((failure, error)) => {
                tracing::warn!(
                    router = %router.display_name,
                    reason = %failure,
                    error = %error,
                    "semantic classifier call failed; applying classifier.on_failure",
                );
                Err(failure)
            }
        },
        None => {
            decide_by_embedding(
                state,
                snapshot,
                router_entry,
                auth,
                semantic,
                prompt,
                request_id,
                parent_counted,
            )
            .await
        }
    };
    let pick = match pick {
        Ok(pick) => pick,
        Err(failure) => {
            note_decision(None, None, Some(SemanticFallback::DecisionFailed));
            return fallback(
                state, snapshot, router, source_ip, groups, semantic, failure,
            );
        }
    };

    let (attempt, route_name): (AttemptModel, Option<String>) = match pick.winner {
        Some(i) => {
            let (attempt, fell_back) = select_eligible(
                state,
                snapshot,
                router,
                source_ip,
                groups,
                &semantic.routes[i].target_ref(snapshot),
                Some(&default_target),
            )?;
            // `x-aisix-route` reports the route that actually served the
            // request: a winner displaced by its target's gates is a
            // fall-through to `default`, not a served route.
            let name = (!fell_back).then(|| semantic.routes[i].name.clone());
            note_decision(
                name.clone(),
                pick.score,
                fell_back.then_some(SemanticFallback::TargetUnavailable),
            );
            (attempt, name)
        }
        None => {
            note_decision(None, pick.score, pick.miss);
            let (attempt, _) = select_eligible(
                state,
                snapshot,
                router,
                source_ip,
                groups,
                &default_target,
                None,
            )?;
            (attempt, None)
        }
    };
    tracing::debug!(
        router = %router_entry.value.display_name,
        resolved_route = ?route_name,
        target = %attempt.model.display_name,
        "semantic routing decision",
    );
    Ok((vec![attempt], route_name, pick.embed_spend))
}

fn note_decision(route: Option<String>, score: Option<f64>, fallback: Option<SemanticFallback>) {
    crate::attribution::note_semantic_decision(SemanticAccessLog {
        route,
        score,
        fallback,
    });
}

/// The embedding decision: embed the prompt, score it against every
/// route's examples. `Err` when the embedding could not be computed, which
/// the caller answers with `on_embedding_failure`.
#[allow(clippy::too_many_arguments)]
async fn decide_by_embedding(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    router_entry: &ResourceEntry<Model>,
    auth: &crate::auth::AuthenticatedKey,
    semantic: &Semantic,
    prompt: &str,
    request_id: &str,
    parent_counted: &mut Vec<String>,
) -> Result<Pick, DecisionFailure> {
    // Resolve the embedding model + its modality metadata. A dangling or
    // wrong-kind reference is a config error; degrade via the failure
    // policy rather than 500.
    let embedding_model = semantic.embedding_model_ref(snapshot);
    let embed_entry = match snapshot.models.get_by_name(&embedding_model) {
        Some(e) if e.value.is_embedding() => e,
        other => {
            tracing::warn!(
                router = %router_entry.value.display_name,
                embedding_model = %embedding_model,
                found = other.is_some(),
                "semantic router references a missing or non-embedding embedding_model; \
                 applying on_embedding_failure",
            );
            return Err(DecisionFailure::EmbeddingUnavailable);
        }
    };
    let dims = embed_entry
        .value
        .embedding
        .as_ref()
        .map(|e| e.dimensions)
        .unwrap_or(0);

    let vector_scope = SemanticVectorCache::vector_scope(&embed_entry.id, &embed_entry.value);

    // Assemble the prompt (index 0) with every uncached example text as one
    // logical embedding batch. Providers without batch input may split it
    // into multiple upstream calls. Steady state: only the prompt is uncached.
    let mut pending: Vec<String> = Vec::new();
    for route in &semantic.routes {
        for ex in &route.examples {
            if state.semantic_cache.get(&vector_scope, dims, ex).is_none()
                && !pending.iter().any(|p| p == ex)
            {
                pending.push(ex.clone());
            }
        }
    }
    let mut to_embed: Vec<String> = Vec::with_capacity(1 + pending.len());
    to_embed.push(prompt.to_string());
    to_embed.extend(pending.iter().cloned());

    // Deadline chain for the embed sub-call: the router-level knob wins,
    // then the embedding model's own `timeout`, then the deployment
    // default — previously only the router knob applied, so a hung
    // embedding upstream stalled every semantic request unbounded.
    let embed_deadline = semantic.embedding_timeout().or_else(|| {
        crate::routing::effective_timeouts(&embed_entry.value, None, state.default_timeouts).request
    });
    // 分类那一发是一次真实的分派：它打的是嵌入模型自己的上游，花的是真钱。
    // 所以它也要过那个模型自己的闸 —— 内联的 TPM/TPD、以及跟着模型走的
    // `scope: model` 策略（含花费上限）。这条路不经 `resolve_attempt_models`
    // 那个主干，所以只接在主干上的闸在这里是静默缺席的。
    //
    // 被闸拒不当成错误抛给调用方，而是并入既有的「嵌入不可用」那条路：走
    // `on_embedding_failure`。语义路由本来就为这种情况准备了降级出口，而把
    // 「分类模型限流了」变成整条请求 429 会让一个保护性上限变成停服。
    //
    // 带上父级：调用方寻址的是这个语义路由，所以按路由名写的策略条件也该
    // 覆盖它的分类子调用（#1267）—— 额度用完时分类调用根本不发出去。这类
    // 策略按调用方请求只计一次请求数：这里记下它们，chat 目标阶段不再重计。
    let embed_reservation = match crate::quota::reserve_semantic_classifier(
        state,
        snapshot,
        auth,
        &embed_entry.value.display_name,
        &embed_entry.id,
        &embed_entry.value,
        crate::quota::RoutingParent {
            name: &router_entry.value.display_name,
            entry_id: &router_entry.id,
        },
        parent_counted,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                router = %router_entry.value.display_name,
                embedding_model = %embed_entry.value.display_name,
                error = %e,
                "semantic classifier refused by its own limits; applying \
                 on_embedding_failure",
            );
            return Err(DecisionFailure::EmbeddingUnavailable);
        }
    };
    let embedded = match embed_texts(
        &state.hub,
        snapshot,
        &embed_entry,
        embed_deadline,
        request_id,
        GatewayEmbeddingPurpose::SemanticRoute,
        &to_embed,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            let failure = DecisionFailure::of_embedding_error(&e);
            tracing::warn!(
                router = %router_entry.value.display_name,
                reason = %failure,
                error = %embed_failure_text(&e),
                "semantic embedding call failed; applying on_embedding_failure",
            );
            return Err(failure);
        }
    };

    let embed_spend =
        record_embed_usage(state, snapshot, &embed_entry, auth, embedded.prompt_tokens);
    // 提交到嵌入模型自己的那些桶。父级那边的记账是另一笔：`embed_spend` 会被
    // 调用方加进父级的提交里，两笔各记各的层，互不替代。
    embed_reservation
        .commit(u64::from(embedded.prompt_tokens), embed_spend)
        .await;
    let mut iter = embedded.vectors.into_iter();
    let prompt_vec = iter
        .next()
        .expect("embed_texts returns at least the prompt vector");
    for text in &pending {
        if let Some(v) = iter.next() {
            state
                .semantic_cache
                .insert(&vector_scope, dims, text, Arc::new(v));
        }
    }

    let route_vecs: Vec<Vec<Arc<Vec<f32>>>> = semantic
        .routes
        .iter()
        .map(|route| {
            route
                .examples
                .iter()
                .filter_map(|ex| state.semantic_cache.get(&vector_scope, dims, ex))
                .collect()
        })
        .collect();

    let decision = decide(semantic, &prompt_vec, &route_vecs);
    // The winner's score, or with no winner the closest miss: the number an
    // operator tunes a threshold against.
    let score = match decision.winner {
        Some(i) => decision.scores.get(i).copied(),
        None => decision.scores.iter().copied().reduce(f32::max),
    };
    Ok(Pick {
        winner: decision.winner,
        score: score.map(log_score),
        miss: decision
            .winner
            .is_none()
            .then_some(SemanticFallback::NoMatch),
        embed_spend,
    })
}

/// A cosine score as the access log writes it. Widening an `f32` exposes
/// its binary representation (`0.8` becomes `0.800000011920929`), which
/// reads as precision the score does not have.
fn log_score(score: f32) -> f64 {
    (f64::from(score) * 1e6).round() / 1e6
}

/// Apply the router's failure policy: route to the fallback target, or
/// surface `503` when the policy is `fail`.
fn fallback(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    router: &Model,
    source_ip: &str,
    groups: &[String],
    semantic: &Semantic,
    failure: DecisionFailure,
) -> Result<(Vec<AttemptModel>, Option<String>, u64), ProxyError> {
    match failure_target(snapshot, semantic) {
        Some(alias) => {
            let (attempt, _) =
                select_eligible(state, snapshot, router, source_ip, groups, &alias, None)?;
            // 决策没有做成，上游没有报回 usage —— 没有可计的花费。
            Ok((vec![attempt], None, 0))
        }
        None => Err(ProxyError::SemanticDecisionFailed {
            router: router.display_name.clone(),
            reason: failure,
        }),
    }
}

/// Endpoint a `typesafe` Provider Key reaches when it sets no `api_base`.
pub(crate) const TYPESAFE_DEFAULT_API_BASE: &str = "https://api.typesafe.ai";

/// The one question every classifier call asks. The wording is the one the
/// classifier mode was validated with; change it only against a fresh
/// evaluation.
const CLASSIFIER_QUESTION: &str = "route";
const CLASSIFIER_INSTRUCTIONS: &str =
    "Which route should handle this user request to an AI assistant?";

#[derive(serde::Serialize)]
struct ClassifierRequest<'a> {
    state: &'a str,
    model: &'a str,
    questions: ClassifierQuestions<'a>,
}

struct ClassifierQuestions<'a>(&'a [SemanticRoute]);

impl serde::Serialize for ClassifierQuestions<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        #[derive(serde::Serialize)]
        struct Choice<'a> {
            r#type: &'static str,
            instructions: &'static str,
            criteria: Criteria<'a>,
        }
        let mut map = s.serialize_map(Some(1))?;
        map.serialize_entry(
            CLASSIFIER_QUESTION,
            &Choice {
                r#type: "choice",
                instructions: CLASSIFIER_INSTRUCTIONS,
                criteria: Criteria(self.0),
            },
        )?;
        map.end()
    }
}

/// The options: exactly the configured routes, in configured order — which
/// a `serde_json::Map` (sorted by key) would not keep. The gateway adds no
/// option of its own; a catch-all is a route the operator configures.
struct Criteria<'a>(&'a [SemanticRoute]);

impl serde::Serialize for Criteria<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for route in self.0 {
            map.serialize_entry(
                &route.name,
                route.description.as_deref().unwrap_or_default(),
            )?;
        }
        map.end()
    }
}

#[derive(serde::Deserialize)]
struct ClassifierResponse {
    #[serde(default)]
    answers: std::collections::HashMap<String, ClassifierAnswer>,
}

#[derive(serde::Deserialize)]
struct ClassifierAnswer {
    choice: String,
    confidence: f64,
}

/// Ask the classifier which route fits `prompt`. Returns the index of the
/// route it picked and its confidence.
/// Every way the call can fail comes back as `Err` with a reason for the
/// log, and the caller applies `classifier.on_failure`.
///
/// Detached, like the embedding sub-call: it is a call the GATEWAY decided
/// to make, not one the caller addressed, and it is not metered as one.
async fn classify(
    snapshot: &AisixSnapshot,
    routes: &[SemanticRoute],
    classifier: &SemanticClassifier,
    prompt: &str,
) -> Result<(usize, f64), (DecisionFailure, String)> {
    crate::attribution::detached(classify_inner(snapshot, routes, classifier, prompt)).await
}

async fn classify_inner(
    snapshot: &AisixSnapshot,
    routes: &[SemanticRoute],
    classifier: &SemanticClassifier,
    prompt: &str,
) -> Result<(usize, f64), (DecisionFailure, String)> {
    use DecisionFailure::{ClassifierInvalidResponse, ClassifierUnavailable, ClassifierUpstream};
    let pk = snapshot
        .provider_keys
        .get_by_id(&classifier.provider_key_id)
        .ok_or_else(|| {
            (
                ClassifierUnavailable,
                format!(
                    "classifier provider_key_id {:?} matches no provider key",
                    classifier.provider_key_id
                ),
            )
        })?;
    if pk.value.provider != TYPESAFE_PROVIDER {
        return Err((
            ClassifierUnavailable,
            format!(
                "classifier provider key {:?} has provider {:?}, not {TYPESAFE_PROVIDER:?}",
                pk.value.display_name, pk.value.provider
            ),
        ));
    }
    let base = pk
        .value
        .api_base
        .as_deref()
        .filter(|b| !b.is_empty())
        .unwrap_or(TYPESAFE_DEFAULT_API_BASE)
        .trim_end_matches('/');
    let body = serde_json::to_vec(&ClassifierRequest {
        state: prompt,
        model: &classifier.model,
        questions: ClassifierQuestions(routes),
    })
    .map_err(|e| {
        (
            ClassifierUnavailable,
            format!("encoding the classifier request: {e}"),
        )
    })?;
    aisix_gateway::note_sent_credential(&pk.value.api_key);
    let resp = crate::http_client::client_for(pk.value.upstream_connection().as_ref())
        .post(format!("{base}/v1/systemone"))
        .bearer_auth(&pk.value.api_key)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .timeout(classifier.timeout())
        .body(body)
        .send()
        .await
        .map_err(|e| DecisionFailure::of_classifier_transport(&e))?;
    let status = resp.status();
    if !status.is_success() {
        return Err((
            ClassifierUpstream,
            format!("classifier returned HTTP {}", status.as_u16()),
        ));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| DecisionFailure::of_classifier_transport(&e))?;
    // Neither error below repeats what the classifier answered: its
    // response can quote the prompt it was sent (`state`), and these
    // errors end up on the router's warn line.
    let mut parsed: ClassifierResponse = serde_json::from_slice(&bytes).map_err(|e| {
        (
            ClassifierInvalidResponse,
            format!(
                "classifier response is not the expected shape (line {} column {})",
                e.line(),
                e.column()
            ),
        )
    })?;
    let answer = parsed.answers.remove(CLASSIFIER_QUESTION).ok_or((
        ClassifierInvalidResponse,
        "classifier response carries no answer to the routing question".to_owned(),
    ))?;
    let picked = routes.iter().position(|r| r.name == answer.choice).ok_or((
        ClassifierInvalidResponse,
        "classifier picked a choice that is not one of the offered routes".to_owned(),
    ))?;
    Ok((picked, answer.confidence))
}

/// Resolve a direct-model alias to the single `AttemptModel` the dispatch
/// loop will drive.
fn attempt_for_target(snapshot: &AisixSnapshot, alias: &str) -> Result<AttemptModel, ProxyError> {
    let entry = snapshot.models.get_by_name(alias).ok_or_else(|| {
        // The alias goes to the log, not the caller: a refusal must not name
        // a router's members (`routing::refused_model_label`).
        tracing::warn!(target_model = %alias, "semantic router target resolves to no model");
        ProxyError::InvalidRequest(
            "semantic router target does not resolve to a known model".to_string(),
        )
    })?;
    Ok(AttemptModel {
        id: entry.id.clone(),
        model: entry.value.clone(),
        priority: 0,
        weight: 1,
    })
}

/// Pick the dispatchable target among `{primary, fallback}` — the
/// semantic single-winner analogue of the group filter
/// (`routing::targets_allowed_for_ip` + `filter_attempt_models`):
///
/// - the API key's provider groups are the first HARD gate: a candidate
///   outside them is dropped, and with none left the caller gets the
///   all-unavailable answer, naming no group and no member — the same as a
///   routing group ([`crate::routing::target_in_groups`]);
/// - the member's client-IP allowlist is a HARD gate: an excluded
///   candidate is dropped, and with every candidate excluded the caller
///   gets the same 403 as the entry gate, naming only the alias they
///   addressed (`routing.rs` does the same for all-excluded groups, so
///   a router never becomes a probe for which members exist);
/// - the member's provider-key budget is a HARD gate too: with every
///   candidate excluded by it, the caller gets the budget's own answer;
/// - member health (request-path cooldown / background-unhealthy) is a
///   SOFT preference: prefer an available candidate, but with none
///   available dispatch the primary anyway — the router has no
///   `when_all_unavailable` knob, and the semantically-right target
///   beats failing the request on an advisory signal.
///
/// Returns the attempt plus whether the fallback displaced the primary
/// (so the caller can clear the served-route attribution).
fn select_eligible(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    router: &Model,
    source_ip: &str,
    allowed_groups: &[String],
    primary: &str,
    fallback_alias: Option<&str>,
) -> Result<(AttemptModel, bool), ProxyError> {
    let mut candidates: Vec<(AttemptModel, bool)> = Vec::with_capacity(2);
    // A dangling alias only matters when nothing else can serve: a route
    // target deleted under a healthy `default` costs the route, not the
    // request, and the same goes for a dangling `default` under a healthy
    // route target.
    let primary_attempt = attempt_for_target(snapshot, primary);
    let unresolved = match primary_attempt {
        Ok(attempt) => {
            candidates.push((attempt, false));
            None
        }
        Err(err) => Some(err),
    };
    if let Some(alias) = fallback_alias {
        if alias != primary {
            if let Ok(attempt) = attempt_for_target(snapshot, alias) {
                candidates.push((attempt, true));
            }
        }
    }
    if let (true, Some(err)) = (candidates.is_empty(), unresolved) {
        return Err(err);
    }
    candidates.retain(|(attempt, _)| {
        crate::routing::target_in_groups(snapshot, &attempt.model, allowed_groups)
    });
    if candidates.is_empty() {
        return Err(crate::routing::no_target_in_groups(&router.display_name));
    }
    candidates.retain(|(attempt, fell_back)| {
        let allowed = attempt.model.ip_allowed(source_ip);
        if !allowed {
            tracing::debug!(
                router = %router.display_name,
                target = %attempt.model.display_name,
                fallback = fell_back,
                "semantic target excluded by its allowed_cidrs",
            );
        }
        allowed
    });
    if candidates.is_empty() {
        return Err(ProxyError::ModelIpRestricted(router.display_name.clone()));
    }
    // A candidate on a provider key whose budget is exhausted is excluded
    // as hard as one outside its allowlist.
    let gate = crate::budget::gate(state, snapshot);
    let mut refusal: Option<ProxyError> = None;
    candidates.retain(
        |(attempt, fell_back)| match gate.check_model(&attempt.model) {
            Ok(()) => true,
            Err(err) => {
                tracing::debug!(
                    router = %router.display_name,
                    target = %attempt.model.display_name,
                    fallback = fell_back,
                    error = %err,
                    "semantic target excluded by its provider key's budget",
                );
                if !matches!(refusal, Some(ProxyError::BudgetExceeded(_))) {
                    refusal = Some(err);
                }
                false
            }
        },
    );
    if let (true, Some(err)) = (candidates.is_empty(), refusal) {
        return Err(err);
    }
    let picked = candidates
        .iter()
        .position(|(attempt, _)| {
            let stale_after = attempt
                .model
                .background_model_check
                .as_ref()
                .map(|cfg| std::time::Duration::from_secs(cfg.stale_after_seconds));
            matches!(
                state
                    .runtime_status
                    .status_with_stale(&attempt.id, stale_after)
                    .status,
                crate::RuntimeStatus::Healthy | crate::RuntimeStatus::NotApplicable
            )
        })
        .unwrap_or(0);
    let (attempt, fell_back) = candidates.swap_remove(picked);
    if fell_back {
        tracing::debug!(
            router = %router.display_name,
            served = %attempt.model.display_name,
            "semantic route target unavailable; serving default",
        );
    }
    Ok((attempt, fell_back))
}

/// Why [`embed_texts`] produced no vectors.
#[derive(Debug)]
pub(crate) enum EmbedTextsError {
    /// The call could not be made, or it failed: the embedding model's
    /// configuration, transport, a timeout, or an upstream error answer.
    Dispatch(ProxyError),
    /// The upstream answered 2xx with vectors that do not fit the request:
    /// the wrong dimension, base64 instead of floats, or the wrong count.
    /// The text names only lengths, never response content.
    UnusableResponse(String),
}

impl From<ProxyError> for EmbedTextsError {
    fn from(err: ProxyError) -> Self {
        Self::Dispatch(err)
    }
}

/// Embed `texts` through the embedding model's bridge in one batched call,
/// returning one float vector per input in input order. Shared by the
/// semantic router (route matching) and the cache gate's semantic layer
/// (`aisix-proxy::chat`), which is why the deadline arrives as a plain
/// `Option<Duration>` rather than either feature's config type.
/// 一次嵌入子调用的结果：向量，外加上游报回来的 token 数。
///
/// 带上 usage 是为了让调用方能给这次调用**定价**。原先只回向量，于是这笔真金
/// 白银的上游调用在所有账上都不存在：不进花费指标、不占任何额度。
#[derive(Debug)]
pub(crate) struct Embedded {
    pub vectors: Vec<Vec<f32>>,
    pub prompt_tokens: u32,
}

pub(crate) async fn embed_texts(
    hub: &aisix_gateway::Hub,
    snapshot: &AisixSnapshot,
    embed_entry: &ResourceEntry<Model>,
    timeout: Option<std::time::Duration>,
    request_id: &str,
    purpose: GatewayEmbeddingPurpose,
    texts: &[String],
) -> Result<Embedded, EmbedTextsError> {
    // Detached: this is a dispatch the GATEWAY decided to make — for a
    // semantic guardrail, a semantic route, or the semantic cache — not one
    // the caller addressed. Two of its three callers run after the winning
    // attempt has already settled, so letting it commit the embedding model
    // to the request's attribution cell would put that model on the
    // request's own access-log line and usage event
    // (see `attribution::detached`).
    crate::attribution::detached(embed_texts_inner(
        hub,
        snapshot,
        embed_entry,
        timeout,
        request_id,
        purpose,
        texts,
    ))
    .await
}

async fn embed_texts_inner(
    hub: &aisix_gateway::Hub,
    snapshot: &AisixSnapshot,
    embed_entry: &ResourceEntry<Model>,
    timeout: Option<std::time::Duration>,
    request_id: &str,
    purpose: GatewayEmbeddingPurpose,
    texts: &[String],
) -> Result<Embedded, EmbedTextsError> {
    let model = &embed_entry.value;
    crate::dispatch::require_provider(model)?;
    let pk_entry = crate::dispatch::resolve_provider_key(snapshot, model)?;
    let bridge = crate::dispatch::resolve_bridge(hub, &pk_entry.value)
        .ok_or(ProxyError::ProviderUnavailable)?;
    let upstream_model = crate::dispatch::require_upstream_model(model)?.to_string();
    let audit_model_id = crate::attribution::capped_gateway_embedding_model_id(&embed_entry.id);
    let dimensions = model.embedding.as_ref().map(|e| e.dimensions);

    let req = EmbeddingRequest {
        extra: Default::default(),
        token_input: None,
        model: upstream_model,
        input: texts.to_vec(),
        input_was_single: texts.len() == 1,
        encoding_format: Some("float".to_string()),
        dimensions,
        input_type: None,
        embedding_types: None,
    };
    let ctx = {
        let base = crate::dispatch::bridge_ctx(
            request_id,
            &embed_entry.id,
            Arc::clone(model),
            &pk_entry.id,
            Arc::clone(&pk_entry.value),
            None,
        );
        match timeout {
            Some(d) => base.with_deadline(d),
            None => base,
        }
    };

    // Keep this guard alive across the bridge await. If the downstream client
    // cancels this parent request while the bridge is pending, its Drop writes
    // a failed child audit directly to the shared parent ledger before the
    // cancel emitter takes that ledger into the terminal 499 event.
    let mut audit_call = crate::attribution::begin_gateway_embedding_call(purpose, &audit_model_id);
    let response = bridge.embed(&req, &ctx).await;
    let latency_ms = audit_call.settle();
    let resp = match response {
        Ok(resp) => {
            crate::attribution::note_gateway_embedding_call(GatewayEmbeddingCall {
                count: 1,
                purpose,
                embedding_model_id: audit_model_id.clone(),
                prompt_tokens: resp.usage.prompt_tokens,
                total_tokens: resp.usage.total_tokens,
                usage_source: match resp.usage.source {
                    EmbeddingUsageSource::Reported => GatewayEmbeddingUsageSource::Reported,
                    EmbeddingUsageSource::Unavailable => GatewayEmbeddingUsageSource::Unavailable,
                },
                latency_ms,
                outcome: GatewayEmbeddingOutcome::Succeeded,
            });
            resp
        }
        Err(err) => {
            crate::attribution::note_gateway_embedding_call(GatewayEmbeddingCall {
                count: 1,
                purpose,
                embedding_model_id: audit_model_id,
                prompt_tokens: 0,
                total_tokens: 0,
                usage_source: GatewayEmbeddingUsageSource::Unavailable,
                latency_ms,
                outcome: GatewayEmbeddingOutcome::Failed,
            });
            return Err(ProxyError::Bridge(err).into());
        }
    };
    let prompt_tokens = resp.usage.prompt_tokens;
    let mut data = resp.data;
    data.sort_by_key(|d| d.index);
    let expected_dims = dimensions.map(|d| d as usize);
    let mut out = Vec::with_capacity(data.len());
    for obj in data {
        match obj.embedding {
            EmbeddingVector::Float(v) => {
                // Surface a wrong-dimension response explicitly instead of
                // letting cosine similarity fold the length mismatch into
                // 0.0 (which would silently route every request to default).
                if let Some(expected) = expected_dims {
                    if v.len() != expected {
                        return Err(EmbedTextsError::UnusableResponse(format!(
                            "embedding endpoint returned a vector of length {}; expected {expected}",
                            v.len(),
                        )));
                    }
                }
                out.push(v);
            }
            EmbeddingVector::Base64(_) => {
                return Err(EmbedTextsError::UnusableResponse(
                    "embedding endpoint returned base64 vectors; float vectors are required".into(),
                ));
            }
        }
    }
    if out.len() != texts.len() {
        return Err(EmbedTextsError::UnusableResponse(format!(
            "embedding endpoint returned {} vectors for {} inputs",
            out.len(),
            texts.len(),
        )));
    }
    Ok(Embedded {
        vectors: out,
        prompt_tokens,
    })
}

/// 给一次嵌入子调用记账：发一条用量记录，并返回它的花费（micro-USD）。
///
/// **两个系统各计一次，互不重复。** 指标这边按**嵌入模型自己的**标签发，所以
/// 序列标注是对的，按 `user_id` 求和时也就自动算到了这个人头上；额度那边由
/// 调用方把返回的数字并进本次请求的预留提交。
///
/// 记在 `/v1/embeddings` 名下而不是发起它的那个端点：上游确实是一次嵌入调用，
/// 按它归类，成本分析里才看得出「路由本身花了多少」。
pub(crate) fn record_embed_usage(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    embed_entry: &ResourceEntry<Model>,
    auth: &crate::auth::AuthenticatedKey,
    prompt_tokens: u32,
) -> u64 {
    let model = &embed_entry.value;
    let spend = crate::usage_attr::request_spend_micro_usd(
        snapshot,
        &embed_entry.id,
        aisix_core::InputTokens::uncached_only(u64::from(prompt_tokens)),
        0,
    );
    let provider = model
        .provider
        .as_deref()
        .unwrap_or("unknown")
        .to_ascii_lowercase();
    let pk_id = crate::dispatch::resolve_provider_key(snapshot, model)
        .map(|e| e.id.clone())
        .unwrap_or_default();
    let pk = crate::usage_attr::ResolvedPk::resolve(snapshot, &pk_id);
    crate::request_metrics::record_usage(
        state,
        "/v1/embeddings",
        crate::request_metrics::Caller::new(auth),
        crate::request_metrics::Upstream {
            provider: &provider,
            model: &model.display_name,
            upstream_model: model.upstream_model().unwrap_or("unknown"),
            pk: pk.labels(),
            // 这是一次内部子调用：不流式，也谈不上回退。
            stream: false,
            is_fallback: false,
        },
        crate::request_metrics::Tokens {
            input: prompt_tokens,
            output: 0,
            total: prompt_tokens,
            cached: 0,
            cache_read: 0,
            cache_creation: 0,
            spend_usd: spend as f64 / 1_000_000.0,
            // 发起者是网关自己（语义路由的分类调用），不是某个客户端。
            client_type: "gateway",
        },
    );
    spend
}

/// What a failed [`embed_texts`] call may say on a log line.
///
/// The text embedded is a caller's prompt or a guardrail's screened input,
/// and an upstream response can quote it back, so nothing a response said
/// is kept: an error envelope keeps only its status, and a decode failure
/// only the position serde reported (its message quotes the offending
/// value). Everything else — transport, timeout, configuration — keeps
/// its full error chain.
pub(crate) fn embed_failure_text(err: &EmbedTextsError) -> String {
    use aisix_gateway::BridgeError;
    let err = match err {
        EmbedTextsError::Dispatch(err) => err,
        EmbedTextsError::UnusableResponse(message) => return message.clone(),
    };
    match err {
        ProxyError::Bridge(BridgeError::UpstreamStatus { status, .. }) => {
            format!("upstream returned HTTP {status}")
        }
        ProxyError::Bridge(BridgeError::UpstreamInBand { status, .. }) => match status {
            Some(status) => format!("upstream reported an in-band error (status {status})"),
            None => "upstream reported an in-band error".to_owned(),
        },
        ProxyError::Bridge(BridgeError::UpstreamDecode(message)) => {
            match decode_position(message) {
                Some((line, column)) => {
                    format!("upstream response could not be decoded (line {line} column {column})")
                }
                None => "upstream response could not be decoded".to_owned(),
            }
        }
        _ => aisix_guardrails::error_chain(err),
    }
}

/// The `line N column M` suffix serde_json appends to its messages. Only
/// the two numbers are taken, so nothing of the message itself survives.
fn decode_position(message: &str) -> Option<(u64, u64)> {
    let (_, position) = message.rsplit_once(" at line ")?;
    let (line, column) = position.split_once(" column ")?;
    Some((line.parse().ok()?, column.parse().ok()?))
}

#[cfg(test)]
mod embed_failure_text_tests {
    use super::*;
    use aisix_gateway::BridgeError;

    fn logged(err: BridgeError) -> String {
        embed_failure_text(&EmbedTextsError::Dispatch(ProxyError::Bridge(err)))
    }

    #[test]
    fn an_error_envelope_keeps_only_its_status() {
        let err = BridgeError::upstream_status(400, "invalid input: screened text");
        assert_eq!(logged(err), "upstream returned HTTP 400");
    }

    #[test]
    fn a_decode_failure_keeps_only_its_position() {
        let serde = serde_json::from_str::<Vec<f32>>(r#""screened text""#).unwrap_err();
        let message = format!("upstream body: {serde}");
        assert!(message.contains("screened text"), "{message}");
        assert_eq!(
            logged(BridgeError::UpstreamDecode(message)),
            "upstream response could not be decoded (line 1 column 15)"
        );
        assert_eq!(
            logged(BridgeError::UpstreamDecode(
                "error decoding response body".into()
            )),
            "upstream response could not be decoded"
        );
    }

    #[test]
    fn transport_and_configuration_errors_keep_their_text() {
        assert!(logged(BridgeError::Transport("connection refused".into()))
            .contains("connection refused"));
        let config =
            BridgeError::Config("token mint upstream returned HTTP 400: invalid_grant".into());
        assert!(logged(config).contains("invalid_grant"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aisix_core::resource::ResourceEntry;

    fn semantic(json: &str) -> Semantic {
        serde_json::from_str(json).unwrap()
    }

    fn arc(v: Vec<f32>) -> Arc<Vec<f32>> {
        Arc::new(v)
    }

    fn router() -> Semantic {
        semantic(
            r#"{
                "embedding_model": "bge-m3",
                "routes": [
                    {"name": "legal", "target": "opus", "examples": ["a"], "threshold": 0.8},
                    {"name": "code",  "target": "sonnet", "examples": ["b"]}
                ],
                "default": "gpt-4o",
                "match": {"threshold": 0.5}
            }"#,
        )
    }

    /// A row repointed at another embedding model of the same dimensions must
    /// not keep scoring against the old model's vectors: cosines across two
    /// embedding spaces are noise, and nothing would report it.
    #[test]
    fn an_example_vector_does_not_survive_a_repointed_embedding_row() {
        let row = |model_name: &str, key: &str| -> Model {
            serde_json::from_value(serde_json::json!({
                "display_name": "bge-m3",
                "provider": "openai",
                "model_name": model_name,
                "provider_key_id": key,
                "embedding": {"dimensions": 2},
            }))
            .unwrap()
        };
        let cache = SemanticVectorCache::default();
        let scope = SemanticVectorCache::vector_scope("m-embed", &row("old", "pk-1"));
        cache.insert(&scope, 2, "an example", arc(vec![1.0, 0.0]));
        assert!(cache.get(&scope, 2, "an example").is_some());
        for moved in [row("new", "pk-1"), row("old", "pk-2")] {
            let scope = SemanticVectorCache::vector_scope("m-embed", &moved);
            assert!(cache.get(&scope, 2, "an example").is_none());
        }
    }

    fn select_snapshot(names: &[&str]) -> AisixSnapshot {
        let snap = AisixSnapshot::new();
        for name in names {
            let model: Model = serde_json::from_value(serde_json::json!({
                "display_name": name,
                "provider": "openai",
                "model_name": "up",
                "provider_key_id": "pk-1",
            }))
            .unwrap();
            snap.models
                .insert(ResourceEntry::new(format!("id-{name}"), model, 1));
        }
        snap
    }

    fn select_state(snap: &AisixSnapshot) -> ProxyState {
        ProxyState::new(
            aisix_core::snapshot::SnapshotHandle::new(snap.clone()),
            Arc::new(aisix_gateway::Hub::new()),
            &aisix_core::ProxyConfig {
                addr: "127.0.0.1:0".into(),
                request_body_limit_bytes: 0,
                tls: None,
                real_ip: Default::default(),
                request_id: Default::default(),
                thread_per_core: None,
                workers: None,
                url_rewrites: Vec::new(),
                expose_debug_headers: false,
                platform: Default::default(),
                listeners: Vec::new(),
            },
        )
    }

    fn router_model() -> Model {
        serde_json::from_value(serde_json::json!({
            "display_name": "router",
            "semantic": {
                "embedding_model": "bge-m3",
                "routes": [{"name": "r", "target": "t", "examples": ["a"]}],
                "default": "d",
                "match": {"threshold": 0.5},
            },
        }))
        .unwrap()
    }

    /// A route target deleted under a healthy `default` costs the route, not
    /// the request.
    #[test]
    fn a_deleted_route_target_falls_through_to_the_fallback() {
        let snap = select_snapshot(&["default-target"]);
        let state = select_state(&snap);
        let (attempt, fell_back) = select_eligible(
            &state,
            &snap,
            &router_model(),
            "127.0.0.1",
            &[],
            "deleted-target",
            Some("default-target"),
        )
        .unwrap();
        assert_eq!(attempt.model.display_name, "default-target");
        assert!(fell_back);
    }

    /// A router reaches its targets past the routing-group path, so it
    /// applies the key's provider groups itself: a target outside them is
    /// not a candidate, the default stands in only when it is inside them,
    /// and with neither inside the caller gets the all-unavailable answer
    /// naming no group, provider key or target.
    #[test]
    fn a_semantic_route_never_picks_a_target_outside_the_keys_groups() {
        let snap = AisixSnapshot::new();
        for (name, pk_id) in [
            ("route-target", "pk-id-beta"),
            ("default-target", "pk-id-alpha"),
        ] {
            let model: Model = serde_json::from_value(serde_json::json!({
                "display_name": name,
                "provider": "openai",
                "model_name": "up",
                "provider_key_id": pk_id,
            }))
            .unwrap();
            snap.models
                .insert(ResourceEntry::new(format!("id-{name}"), model, 1));
        }
        for (id, name, group) in [
            ("pk-id-alpha", "pk-alpha", "grp-alpha"),
            ("pk-id-beta", "pk-beta", "grp-beta"),
        ] {
            let pk: aisix_core::ProviderKey = serde_json::from_value(serde_json::json!({
                "display_name": name,
                "api_key": "sk-x",
                "groups": [group],
            }))
            .unwrap();
            snap.provider_keys.insert(ResourceEntry::new(id, pk, 1));
        }
        let state = select_state(&snap);
        let pick = |allowed: &[&str]| {
            let allowed: Vec<String> = allowed.iter().map(|g| g.to_string()).collect();
            select_eligible(
                &state,
                &snap,
                &router_model(),
                "127.0.0.1",
                &allowed,
                "route-target",
                Some("default-target"),
            )
        };

        let (attempt, fell_back) = pick(&[]).unwrap();
        assert_eq!(attempt.model.display_name, "route-target");
        assert!(!fell_back);

        let (attempt, fell_back) = pick(&["grp-alpha"]).unwrap();
        assert_eq!(attempt.model.display_name, "default-target");
        assert!(fell_back);

        let err = pick(&["grp-zeta"]).map(|_| ()).unwrap_err();
        assert!(
            matches!(
                err,
                ProxyError::AllCandidatesUnavailable {
                    retry_after_secs: None
                }
            ),
            "{err:?}"
        );
        let text = format!("{err} {err:?}");
        for secret in [
            "grp-alpha",
            "grp-beta",
            "grp-zeta",
            "pk-alpha",
            "pk-beta",
            "route-target",
            "default-target",
        ] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
    }

    /// With nothing left to serve it, the refusal names no member.
    #[test]
    fn a_router_with_no_target_left_is_refused_without_naming_one() {
        let snap = select_snapshot(&[]);
        let state = select_state(&snap);
        let err = select_eligible(
            &state,
            &snap,
            &router_model(),
            "127.0.0.1",
            &[],
            "secret-target",
            Some("secret-default"),
        )
        .map(|_| ())
        .unwrap_err();
        assert!(matches!(err, ProxyError::InvalidRequest(_)), "{err:?}");
        let text = err.to_string();
        assert!(!text.contains("secret-target"), "{text}");
        assert!(!text.contains("secret-default"), "{text}");
    }

    /// Embedding is a dispatch the GATEWAY decides to make, and two of its
    /// three callers — a semantic guardrail's OUTPUT hook, and the semantic
    /// cache's write — run after the winning attempt has already settled.
    ///
    /// The request's attribution cell records the last target
    /// `resolve_provider_key` committed to, and the access-log line and the
    /// cancelled-request usage event both read it. So without a cell of its
    /// own, an ordinary request that happens to run a semantic guardrail
    /// would report the EMBEDDING model as the upstream it dispatched to —
    /// on the very line an operator reads to find out which member of a
    /// routing group served them (AISIX-Cloud#1571). Wrong, not merely
    /// absent, which is the worse of the two.
    #[tokio::test]
    async fn embedding_does_not_overwrite_the_caller_s_target() {
        use aisix_core::snapshot::ResourceTable;

        let embed_model: Model = serde_json::from_value(serde_json::json!({
            "display_name": "bge-m3",
            "provider": "openai",
            "model_name": "text-embedding-3-small",
            "provider_key_id": "pk-embed",
        }))
        .unwrap();
        let pk: aisix_core::ProviderKey = serde_json::from_value(serde_json::json!({
            "display_name": "embed-key",
            "secret": "sk-embed",
            "api_base": "http://127.0.0.1:1",
            "provider": "openai",
            "adapter": "openai",
        }))
        .unwrap();
        let provider_keys = ResourceTable::default();
        provider_keys.insert(ResourceEntry::new("pk-embed", pk, 1));
        let snapshot = AisixSnapshot {
            provider_keys,
            ..Default::default()
        };
        let embed_entry = ResourceEntry::new("m-embed", embed_model, 1);

        let cell = std::sync::Arc::new(crate::attribution::RequestAttribution::default());
        crate::attribution::scope(cell.clone(), async {
            // What the CALLER addressed and the gateway dispatched to.
            let served: Model = serde_json::from_value(serde_json::json!({
                "display_name": "served-by",
                "provider": "anthropic",
                "model_name": "claude-sonnet-4",
                "provider_key_id": "pk-chat",
            }))
            .unwrap();
            crate::attribution::note_target(&served, "pk-chat");

            // An empty `Hub` means no bridge, so this fails — but only
            // AFTER `resolve_provider_key` has committed the embedding
            // target, which is the write under test.
            let err = embed_texts(
                &aisix_gateway::Hub::new(),
                &snapshot,
                &embed_entry,
                None,
                "req-embed",
                GatewayEmbeddingPurpose::SemanticRoute,
                &["scan me".to_string()],
            )
            .await
            .expect_err("premise: no bridge is registered, so this must fail");
            assert!(
                matches!(
                    err,
                    EmbedTextsError::Dispatch(ProxyError::ProviderUnavailable)
                ),
                "premise: it must fail at bridge resolution, i.e. after the \
                 provider key was resolved — got {err:?}",
            );

            let resolved = crate::attribution::current().expect("in scope");
            assert_eq!(resolved.upstream_model, "claude-sonnet-4");
            assert_eq!(resolved.provider, "anthropic");
            assert_eq!(resolved.provider_key_id, "pk-chat");
        })
        .await;
    }

    #[test]
    fn decide_picks_highest_route_clearing_its_threshold() {
        let s = router();
        let req = vec![1.0, 0.0];
        // legal example == request (cos 1.0 ≥ 0.8 ✓); code example orthogonal (0.0 < 0.5).
        let route_vecs = vec![vec![arc(vec![1.0, 0.0])], vec![arc(vec![0.0, 1.0])]];
        let d = decide(&s, &req, &route_vecs);
        assert_eq!(d.winner, Some(0));
        assert!((d.scores[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn decide_falls_through_to_default_when_none_clear() {
        let s = router();
        let req = vec![1.0, 0.0];
        // Both examples weakly aligned: legal 0.6 (< 0.8 own threshold),
        // code 0.4 (< 0.5 router threshold) → no winner.
        let route_vecs = vec![
            vec![arc(vec![0.6, 0.8])],   // cos ≈ 0.6
            vec![arc(vec![0.4, 0.917])], // cos ≈ 0.4
        ];
        let d = decide(&s, &req, &route_vecs);
        assert_eq!(d.winner, None);
    }

    #[test]
    fn decide_uses_router_threshold_when_route_has_no_override() {
        let s = router();
        let req = vec![1.0, 0.0];
        // code route (no override → 0.5): give it cos ≈ 0.71 (clears 0.5).
        // legal route (0.8): give it cos ≈ 0.71 (does NOT clear 0.8).
        let route_vecs = vec![vec![arc(vec![1.0, 1.0])], vec![arc(vec![1.0, 1.0])]];
        let d = decide(&s, &req, &route_vecs);
        assert_eq!(
            d.winner,
            Some(1),
            "only the code route clears its threshold"
        );
    }

    #[test]
    fn decide_max_aggregation_takes_best_example() {
        let s = router();
        let req = vec![1.0, 0.0];
        // legal has two examples: one orthogonal (0.0), one identical (1.0).
        // max → 1.0 clears 0.8.
        let route_vecs = vec![
            vec![arc(vec![0.0, 1.0]), arc(vec![1.0, 0.0])],
            vec![arc(vec![0.0, 1.0])],
        ];
        let d = decide(&s, &req, &route_vecs);
        assert_eq!(d.winner, Some(0));
        assert!((d.scores[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn failure_target_maps_each_policy() {
        let snap = AisixSnapshot::default();
        let default_policy = router();
        assert_eq!(
            failure_target(&snap, &default_policy).as_deref(),
            Some("gpt-4o")
        );

        let fail = semantic(
            r#"{"embedding_model":"e","routes":[{"name":"a","target":"m","examples":["x"]}],
                "default":"d","match":{"threshold":0.5},"on_embedding_failure":"fail"}"#,
        );
        assert!(failure_target(&snap, &fail).is_none());

        let target = semantic(
            r#"{"embedding_model":"e","routes":[{"name":"a","target":"m","examples":["x"]}],
                "default":"d","match":{"threshold":0.5},"on_embedding_failure":{"target":"safe"}}"#,
        );
        assert_eq!(failure_target(&snap, &target).as_deref(), Some("safe"));
    }

    /// The id spelling decides at both `on_embedding_failure` shapes, and
    /// resolves against the live table — so a rename of the fallback model
    /// needs no edit to the router.
    #[test]
    fn failure_target_follows_the_id_spelling() {
        let snap = AisixSnapshot::default();
        snap.models.insert(ResourceEntry::new(
            "m-safe",
            serde_json::from_str::<Model>(
                r#"{"display_name":"safe-v2","provider":"openai","model_name":"gpt-4o",
                    "provider_key_id":"11111111-1111-1111-1111-111111111111"}"#,
            )
            .unwrap(),
            1,
        ));

        let explicit = semantic(
            r#"{"embedding_model":"e","routes":[{"name":"a","target":"m","examples":["x"]}],
                "default":"d","match":{"threshold":0.5},
                "on_embedding_failure":{"target":"stale","target_id":"m-safe"}}"#,
        );
        assert_eq!(failure_target(&snap, &explicit).as_deref(), Some("safe-v2"));

        let by_default = semantic(
            r#"{"embedding_model":"e","routes":[{"name":"a","target":"m","examples":["x"]}],
                "default":"stale","default_id":"m-safe","match":{"threshold":0.5}}"#,
        );
        assert_eq!(
            failure_target(&snap, &by_default).as_deref(),
            Some("safe-v2")
        );

        // An id that resolves to nothing stands in as its own name, which
        // dispatches nowhere — what a dangling alias already does.
        let dangling = semantic(
            r#"{"embedding_model":"e","routes":[{"name":"a","target":"m","examples":["x"]}],
                "default":"d","default_id":"m-gone","match":{"threshold":0.5}}"#,
        );
        let resolved = failure_target(&snap, &dangling).unwrap();
        assert_eq!(resolved, "m-gone");
        assert!(snap.models.get_by_name(&resolved).is_none());
    }

    #[test]
    fn cache_round_trips_and_dimension_change_invalidates() {
        let cache = SemanticVectorCache::default();
        assert!(cache.get("bge", 1024, "hello").is_none());
        cache.insert("bge", 1024, "hello", arc(vec![1.0, 2.0]));
        assert_eq!(
            cache.get("bge", 1024, "hello").unwrap().as_slice(),
            &[1.0, 2.0]
        );
        // Different dimensions → different key → miss (auto-invalidation).
        assert!(cache.get("bge", 512, "hello").is_none());
        // Different embedding model → miss.
        assert!(cache.get("other", 1024, "hello").is_none());
    }
}
