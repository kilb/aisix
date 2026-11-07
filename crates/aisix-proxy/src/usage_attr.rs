//! Per-ProviderKey telemetry attribution shared by every request handler's
//! usage-event emitter (#867 + non-chat parity follow-up).
//!
//! The five attribution fields — `provider_kind` / `provider_featured` /
//! `branded_provider` / `pk_label` / `byo_label` — are sourced from the
//! resolved ProviderKey's `telemetry_tags` at emit time. Centralising the
//! snapshot lookup AND the wire-field mapping here keeps the handler family
//! (chat / messages / responses / completions / embeddings / rerank / audio /
//! images) from drifting apart again — the exact bug #867 fixed for
//! `/v1/responses` after it had already been fixed for chat + messages.
//!
//! [`ResolvedPk`] is the second half of that anti-drift move (#941): the
//! attempt's ProviderKey row is looked up ONCE per completion and the
//! result is handed to every terminal emitter, so the metric label and
//! the usage-event attribution can neither disagree nor pay for the same
//! `DashMap` read three times.

use std::borrow::Cow;
use std::sync::Arc;

use aisix_core::{AisixSnapshot, ProviderKey, ResourceEntry};
use aisix_obs::{UsageEvent, UsageEventLabels};

use crate::chat::sanitize_tag;
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::operation::Surface;
use crate::state::ProxyState;

/// The request's ENFORCE-mode guardrail audit handle (AISIX-Cloud#1330),
/// cloned off the resolved chain with
/// [`GuardrailChain::audit_log`](aisix_guardrails::GuardrailChain::audit_log)
/// at the point the chain is resolved.
///
/// Threaded rather than re-derived because the read has to happen at
/// terminal-event time, which is routinely a different frame from the
/// chain's: a streaming emitter runs from a `move` closure after the
/// handler returned, and `chat.rs` erases its chain to `Arc<dyn Guardrail>`
/// before the output hook even runs. `None` — no chain resolved, the
/// dominant guardrail-free deployment — reads identically to an empty
/// snapshot, so no call site needs to branch on it.
pub(crate) type GuardrailAudit = Option<Arc<aisix_guardrails::GuardrailAuditLog>>;

/// Snapshot `audit` into a terminal `UsageEvent`'s
/// `guardrail_enforced_hits`.
///
/// Terminal events only: guardrails run once per request, not once per
/// attempt, so stamping a per-attempt event would report the same hit
/// once per retry.
///
/// Reading does not consume the log, and that is load-bearing rather than
/// incidental: the retrying families call this from emitters that also
/// serve superseded attempts, and a destructive read would leave whichever
/// event happened to go out first holding the only copy.
pub(crate) fn enforced_hits(audit: &GuardrailAudit) -> Vec<aisix_core::GuardrailEnforcedHit> {
    audit.as_ref().map(|a| a.snapshot()).unwrap_or_default()
}

/// Snapshot `audit` into a `UsageEvent`'s `applied_guardrails`: the
/// `{kind, hook}` set of the chain the request resolved.
///
/// The failure path reads it here rather than off the chain because the
/// chain is resolved inside a dispatch frame the `Err` has already left,
/// and a guardrail refusal — the one failure whose row an operator is
/// certain to open — would otherwise claim that no guardrail governed it.
pub(crate) fn applied_guardrails(audit: &GuardrailAudit) -> Vec<aisix_core::AppliedGuardrail> {
    audit
        .as_ref()
        .map(|a| a.applied().to_vec())
        .unwrap_or_default()
}

/// Snapshot `audit` into a `UsageEvent`'s `guardrail_bypassed_reason`:
/// the bounded tag of the first guardrail this request failed OPEN on,
/// empty when none did.
///
/// Not terminal-only, unlike [`enforced_hits`]. A bypass says the request
/// went upstream unscreened, which is true of every attempt the request
/// made, and `chat.rs` has stamped it on its per-attempt events since the
/// field existed — a retry that dropped it would read as a screened
/// attempt.
///
/// `chat.rs` keeps threading its own copy instead of calling this: it
/// hands the value to per-attempt and ensemble sub-call emitters at
/// points where the value is deliberately the one captured EARLIER in the
/// request, which a request-scoped snapshot cannot express.
pub(crate) fn bypass_reason(audit: &GuardrailAudit) -> String {
    audit
        .as_ref()
        .and_then(|a| a.bypass_reason())
        .unwrap_or_default()
}

/// Snapshot `audit` into a terminal `UsageEvent`'s `guardrail_scores`
/// (AISIX-Cloud#1467). Same handle, same terminal-only rule and same
/// non-destructive read as [`enforced_hits`] — a scoring guardrail runs
/// once per request, so a per-attempt event would repeat its numbers once
/// per retry and make a single screening look like several.
pub(crate) fn guardrail_scores(audit: &GuardrailAudit) -> Vec<aisix_core::GuardrailScore> {
    audit
        .as_ref()
        .map(|a| a.score_snapshot())
        .unwrap_or_default()
}

/// [`guardrail_scores`] for the retrying families — see
/// [`terminal_enforced_hits`].
pub(crate) fn terminal_guardrail_scores(
    terminal: bool,
    audit: &GuardrailAudit,
) -> Vec<aisix_core::GuardrailScore> {
    if terminal {
        guardrail_scores(audit)
    } else {
        Vec::new()
    }
}

/// [`enforced_hits`] for the retrying families (chat / messages /
/// responses), whose emitters serve both the terminal event and the
/// superseded per-attempt ones and are told which they are building.
///
/// Written as one helper rather than an `if` at each emitter so the rule
/// — request-scoped attribution rides the terminal event only — is stated
/// once and cannot be applied inconsistently across the three.
pub(crate) fn terminal_enforced_hits(
    terminal: bool,
    audit: &GuardrailAudit,
) -> Vec<aisix_core::GuardrailEnforcedHit> {
    if terminal {
        enforced_hits(audit)
    } else {
        Vec::new()
    }
}

/// Stamp how the caller established its principal onto a usage event.
///
/// The ordinary credential path leaves the field empty (the shape on the
/// wire for the overwhelming majority of events); a principal inherited
/// from an entry's anonymous configuration is marked, so a key that
/// doubles as an anonymous principal can still be told apart in the
/// usage record. Every handler that can serve an anonymous caller —
/// today `/mcp` and the passthrough routes — calls this on its emitter.
pub(crate) fn apply_auth_type(event: &mut UsageEvent, auth: &crate::auth::AuthenticatedKey) {
    if auth.anonymous {
        event.auth_type = "anonymous".to_string();
    }
}

/// The provider's own response object `id`, read off a JSON response body —
/// OpenAI's `chat.completion.id`, a Responses-API `resp_…`, a legacy
/// completions `cmpl-…`, a Cohere rerank `id`.
///
/// #1289 keeps three ids strictly separate: this one, the
/// gateway's own `request_id`, and the provider's HTTP transport header id.
/// None of them may stand in for another, and an absent id stays absent — a
/// synthesised value would send an operator hunting in the provider's console
/// for a call that never happened there. Empty is a normal outcome:
/// embeddings / audio / images carry no id in their response shape at all, an
/// errored call has no response object, and a cache hit never reached a
/// provider.
///
/// The value is upstream-controlled and reaches both a log line and the control plane's
/// `control plane_usage_events`, so it goes through [`sanitize_tag`] (control chars
/// stripped, 256-char cap) — an unescaped newline in an id would otherwise
/// let an upstream forge whole log records.
pub(crate) fn provider_response_id(body: &serde_json::Value) -> String {
    sanitize_provider_response_id(body.get("id").and_then(|v| v.as_str()).unwrap_or_default())
}

/// [`provider_response_id`] for a value already decoded into a typed struct
/// (a bridge `Response.id`, a stream chunk `id`). Every producer must route
/// through one of the two, or the cap and control-char stripping the field's
/// consumers assume are not actually applied to that path.
pub(crate) fn sanitize_provider_response_id(id: &str) -> String {
    if id.is_empty() {
        return String::new();
    }
    sanitize_tag(id.to_string())
}

/// Label value the `provider_key_id` / `provider_key_name` pair falls back
/// to when the request never resolved a ProviderKey. Matches
/// `request_metrics::UNKNOWN` and [`PkLabels::default`].
pub(crate) const UNKNOWN_PK: &str = "unknown";

/// The attempt's ProviderKey, resolved ONCE per completion.
///
/// Three terminal emitters want something off the same row: `record` and
/// `record_usage` want the readable `provider_key_name` label (#890 req-3),
/// and the usage-event emitters want `telemetry_tags` (#867).
/// Each used to look the row up itself — three `DashMap` reads and two
/// `display_name` clones for one request (#941). Resolving here and passing
/// the result down replaces them with one read, and makes the two emits
/// provably agree: they now read the same row observation, not two lookups
/// that a concurrent snapshot swap can separate.
///
/// The borrow is the anti-drift device: [`crate::request_metrics::Upstream`]
/// takes [`PkLabels`], not a bare id, so a new call site cannot reintroduce
/// a per-emit lookup without saying so.
pub(crate) struct ResolvedPk<'a> {
    id: &'a str,
    /// `display_name`, control-char stripped and length-capped via
    /// [`sanitize_tag`]; [`UNKNOWN_PK`] when the id is empty, unresolved
    /// or names a row with a blank display name. Borrowed in the
    /// fallback case so the common pre-dispatch failure allocates nothing.
    name: Cow<'a, str>,
    /// The upstream wire protocol this key dispatches through
    /// (AISIX-Cloud#1403) — [`aisix_gateway::upstream_protocol`] of the
    /// resolved row, [`aisix_gateway::UPSTREAM_PROTOCOL_UNKNOWN`] when
    /// there is no row. Computed HERE rather than at the emit so it
    /// rides along with the id and name the same row produced, on the
    /// one snapshot read this type exists to make.
    protocol: &'static str,
    entry: Option<Arc<ResourceEntry<ProviderKey>>>,
}

impl<'a> ResolvedPk<'a> {
    /// Look the row up once. `id` reaches the metric label verbatim —
    /// including the empty string the pre-dispatch failure paths pass —
    /// so the emitted series is byte-identical to the per-emitter lookups
    /// this replaced.
    pub(crate) fn resolve(snap: &AisixSnapshot, id: &'a str) -> Self {
        let entry = if id.is_empty() {
            None
        } else {
            snap.provider_keys.get_by_id(id)
        };
        let name = match entry.as_ref() {
            Some(e) => {
                let name = sanitize_tag(e.value.display_name.clone());
                if name.is_empty() {
                    Cow::Borrowed(UNKNOWN_PK)
                } else {
                    Cow::Owned(name)
                }
            }
            None => Cow::Borrowed(UNKNOWN_PK),
        };
        let protocol = entry
            .as_ref()
            .map(|e| aisix_gateway::upstream_protocol(&e.value))
            .unwrap_or(aisix_gateway::UPSTREAM_PROTOCOL_UNKNOWN);
        Self {
            id,
            name,
            protocol,
            entry,
        }
    }

    /// A completion that never reached a ProviderKey — the pre-dispatch
    /// rejections and the endpoints that have no upstream key at all.
    /// Same labels the per-emitter lookup produced for an empty id, with
    /// no snapshot read.
    pub(crate) fn unresolved() -> ResolvedPk<'static> {
        ResolvedPk {
            id: "",
            name: Cow::Borrowed(UNKNOWN_PK),
            protocol: aisix_gateway::UPSTREAM_PROTOCOL_UNKNOWN,
            entry: None,
        }
    }

    /// The ProviderKey dimensions of the metric label set.
    pub(crate) fn labels(&self) -> PkLabels<'_> {
        PkLabels {
            id: self.id,
            name: &self.name,
            protocol: self.protocol,
        }
    }

    /// Attribution tags for the usage event. Cloned on demand: the tag
    /// strings only reach the wire on the paths that emit an event, so a
    /// bare metric emit pays nothing for them. An unresolved key yields
    /// the default (all-empty) tags, which skip-serialize to wire NULL.
    pub(crate) fn telemetry_tags(&self) -> aisix_core::TelemetryTags {
        self.entry
            .as_ref()
            .map(|e| e.value.telemetry_tags.clone())
            .unwrap_or_default()
    }
}

/// The ProviderKey dimensions of a metric label set: the id, the
/// readable name, and the upstream wire protocol — all three functions
/// of one ProviderKey row, so the trio adds no series beyond the id.
///
/// The fields are private on purpose. [`ResolvedPk::labels`] and
/// [`PkLabels::default`] are the only ways to build one, so a name or a
/// protocol can never be paired with an id it was not read off — which
/// is the whole reason `Upstream` takes this type instead of a bare id.
#[derive(Clone, Copy)]
pub(crate) struct PkLabels<'a> {
    id: &'a str,
    name: &'a str,
    protocol: &'static str,
}

impl<'a> PkLabels<'a> {
    /// Rebuild the labels from their parts, for a sample recorded after the
    /// `ResolvedPk` that produced them is gone (a parked stream sample).
    pub(crate) fn from_parts(id: &'a str, name: &'a str, protocol: &'static str) -> Self {
        Self { id, name, protocol }
    }

    pub(crate) fn id(self) -> &'a str {
        self.id
    }

    pub(crate) fn name(self) -> &'a str {
        self.name
    }

    /// The `upstream_protocol` label (AISIX-Cloud#1403).
    pub(crate) fn protocol(self) -> &'static str {
        self.protocol
    }
}

impl Default for PkLabels<'_> {
    fn default() -> Self {
        Self {
            id: UNKNOWN_PK,
            name: UNKNOWN_PK,
            protocol: aisix_gateway::UPSTREAM_PROTOCOL_UNKNOWN,
        }
    }
}

/// The exporter set a terminal emit fans out to.
///
/// Deliberately NOT read off the request's frozen snapshot (#941 audit
/// M1). Exporter membership is a delivery-authorization decision, not a
/// label: an operator who deletes an exporter — say one configured for
/// full content capture, pointed at the wrong tenant — must stop it
/// receiving events, including captured prompt and response text, from
/// requests that are still in flight. A frozen list would keep feeding it
/// for as long as the longest request runs.
///
/// Always load current membership, even if the request began before the first
/// exporter was added. Besides making additions immediate, this guarantees
/// fan-out GC never mistakes a stale empty request snapshot for the live set.
pub(crate) struct LiveExporters {
    generation: u64,
    entries: Vec<Arc<ResourceEntry<aisix_core::ObservabilityExporter>>>,
}

impl LiveExporters {
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

impl std::ops::Deref for LiveExporters {
    type Target = [Arc<ResourceEntry<aisix_core::ObservabilityExporter>>];

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

pub(crate) fn live_exporters(
    state: &ProxyState,
    _request_snapshot: &AisixSnapshot,
) -> LiveExporters {
    let view = state.snapshot.load_versioned();
    LiveExporters {
        generation: view.version,
        entries: view.snapshot.observability_exporters.entries(),
    }
}

/// Total token cost of a request as committed against TPM/TPD rate limits
/// (and reported as the prometheus usage total): prompt + completion +
/// Anthropic cache creation/read. Anthropic reports cache tokens as counters
/// SEPARATE from `input_tokens`, so a prompt+completion sum silently
/// undercounts cached traffic — the OpenAI bridge already folds them into
/// `total_tokens` (#679) and the CP display total includes them (#906); this
/// keeps the native `/v1/messages` and `/v1/responses` commits consistent
/// (#995). OpenAI's `cached_tokens` is a subset of
/// `prompt_tokens` and is deliberately NOT an input here.
pub(crate) fn total_tokens_with_cache(
    prompt_tokens: u32,
    completion_tokens: u32,
    cache_creation_tokens: u32,
    cache_read_tokens: u32,
) -> u64 {
    u64::from(prompt_tokens)
        + u64::from(completion_tokens)
        + u64::from(cache_creation_tokens)
        + u64::from(cache_read_tokens)
}

/// The `total_tokens` a JSON `usage` object reports, verbatim — the value
/// `UsageEvent::total_tokens` records (`UsageStats::upstream_total_tokens`).
/// `None` when the object carries none: the event leaves the field unset
/// then, and is never given a total summed here.
pub(crate) fn reported_total_tokens(usage: Option<&serde_json::Value>) -> Option<u32> {
    usage
        .and_then(|u| u.get("total_tokens"))
        .and_then(serde_json::Value::as_u64)
        .map(|n| n.min(u64::from(u32::MAX)) as u32)
}

/// The upstream total of a usage record built from several reports (a
/// realtime session's frames, a batch's output lines): their sum, standing
/// only while every report that counted tokens carried a total.
#[derive(Debug, Default)]
pub(crate) struct ReportedTotalSum {
    sum: u64,
    unreported: bool,
}

impl ReportedTotalSum {
    /// Fold one report in. A report that counted no tokens (a cancelled
    /// response's empty usage) neither adds nor breaks the total.
    pub(crate) fn add(&mut self, usage: &serde_json::Value, counted_tokens: bool) {
        match reported_total_tokens(Some(usage)) {
            Some(total) => self.sum += u64::from(total),
            None if counted_tokens => self.unreported = true,
            None => {}
        }
    }

    /// The value the usage event records; 0 (unset) without a total.
    pub(crate) fn recorded(&self) -> u32 {
        if self.unreported {
            0
        } else {
            self.sum.min(u64::from(u32::MAX)) as u32
        }
    }
}

/// The `model` metric label for a request whose client-supplied `model`
/// field never resolved to a configured model (e.g. model-not-found). See
/// [`metric_model_label`].
pub const UNRESOLVED_MODEL_LABEL: &str = "unresolved";

/// Bound the `model` metric label to the configured set. A request's `model`
/// field is arbitrary caller-controlled text until it resolves against the
/// snapshot; feeding the raw value into a Prometheus label lets a caller
/// explode metric cardinality. Three outcomes:
/// - an exact configured name (direct or virtual router — both live in
///   `models`) labels as itself;
/// - a name only a WILDCARD row serves labels as that row's `display_name`
///   (`openai/*`) — the caller can mint unlimited concrete suffixes, so the
///   configured row is the only bounded identity, and it keeps success and
///   failure series for one row under one label;
/// - anything else is the fixed [`UNRESOLVED_MODEL_LABEL`] sentinel.
///
/// This is the typed-endpoint analogue of passthrough's
/// `PASSTHROUGH_MODEL_LABEL` guard (#451); `request_metrics::record` /
/// `record_usage` apply it at the emit chokepoint so no handler family
/// member can drift.
/// Narrow an upstream-reported token count to the `u32` the usage event and
/// the token metrics carry.
///
/// `as u32` wraps, and wrapping **understates**: a provider reporting
/// `4_294_967_297` would be billed as 1 token. That matters because the
/// upstream is not always a trusted party — a ProviderKey's `api_base` points
/// wherever the operator says, including at a caller's own endpoint — so a
/// count that silently collapses to a plausible small number is a billing
/// integrity hole rather than a rounding detail. Saturating instead pins the
/// ceiling: absurd input stays absurd and visible.
pub(crate) fn token_count(raw: u64) -> u32 {
    u32::try_from(raw).unwrap_or(u32::MAX)
}

/// USD cost for one request, from the price the operator configured on the
/// **dispatched** model (`Model.cost`).
///
/// `model_id` is the winning target's row id, not the caller-addressed entry:
/// `cost` is a direct-model knob, so a routing group's price is whichever
/// member actually served the request.
///
/// Returns `0.0` when the row carries no `cost`. That is deliberate rather
/// than a fallback: a control plane recomputes spend server-side from its own
/// pricing catalog, so a DP-side number is only wanted where the operator
/// asked for one by setting the field. Before this existed the main LLM
/// endpoints always emitted `0.0`, which left an open-source deployment — one
/// with no control plane to recompute anything — with no cost signal at all
/// even after configuring `cost`.
pub(crate) fn request_cost_usd(
    snap: &AisixSnapshot,
    model_id: &str,
    input: aisix_core::InputTokens,
    completion_tokens: u64,
) -> f64 {
    let Some(entry) = snap.models.get_by_id(model_id) else {
        return 0.0;
    };
    pricing_index(snap)
        .resolve(&entry.value)
        .map(|cost| cost.calculate_with_cache(input, completion_tokens))
        .unwrap_or(0.0)
}

/// 这份快照的定价索引：`pricing_key` 先查定价文档，再落到模型自己的 `cost`。
/// 所有读模型价钱的地方都要走它，否则按文档定价的模型在那个地方被算成零。
///
/// 进程级缓存，这样 [`request_cost_usd`] 的几十个调用点不必各带一份 state。
/// 缓存键是两张定价表的 generation，而 generation 是进程级唯一戳，所以不同
/// 快照之间不会串价；命中只是一次原子读。
pub(crate) fn pricing_index(snap: &AisixSnapshot) -> Arc<aisix_core::models::PricingIndex> {
    static INDEX: std::sync::LazyLock<aisix_core::models::LivePricingIndex> =
        std::sync::LazyLock::new(aisix_core::models::LivePricingIndex::new);
    INDEX.for_snapshot(snap)
}

/// USD → micro-USD，花费计数器用的整数单位（1 USD = 1_000_000）。
///
/// 四舍五入而非截断：单次调用常在 1e-4 美元量级，一律截断会让花费系统性
/// 偏低。`max(0.0)` 兜住负价与 NaN（`f64::max` 在一侧为 NaN 时返回另一侧），
/// `as u64` 在 Rust 里是饱和转换，不会回绕。
pub(crate) fn spend_micro_usd(cost_usd: f64) -> u64 {
    (cost_usd * 1_000_000.0).round().max(0.0) as u64
}

/// 这次调用要记进花费桶的量（micro-USD）。与 [`request_cost_usd`] 同源同参，
/// 只是换成计数器用的整数单位——两处各算一遍才是它们漂移的方式。
pub(crate) fn request_spend_micro_usd(
    snap: &AisixSnapshot,
    model_id: &str,
    input: aisix_core::InputTokens,
    completion_tokens: u64,
) -> u64 {
    spend_micro_usd(request_cost_usd(snap, model_id, input, completion_tokens))
}

/// Split the canonical usage counters into the disjoint buckets pricing
/// needs. The ONLY place that arithmetic lives.
///
/// The two upstream shapes disagree about what `prompt_tokens` contains, and
/// `UsageStats` carries both vocabularies side by side:
///
/// - `cached_prompt_tokens` is OpenAI's, a SUBSET of `prompt_tokens`.
/// - `cache_read_tokens` / `cache_creation_tokens` are Anthropic's, counters
///   SEPARATE from `prompt_tokens`.
///
/// - `cache_write_subset` is `UsageStats::cache_write_tokens`, OpenAI's own
///   write count, also a SUBSET of `prompt_tokens`.
///
/// So the fresh portion subtracts only the two subset ones. Getting this
/// backwards is silent in both directions — double-charging the cached half on
/// one provider, or pricing six thousand billed tokens at nothing on the other
/// — and no request fails either way, which is why it is derived once here
/// rather than at each of the ten call sites.
pub(crate) fn input_tokens_for_pricing(
    prompt_tokens: u64,
    cached_prompt_tokens: u64,
    cache_write_subset: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
) -> aisix_core::InputTokens {
    aisix_core::InputTokens {
        uncached: prompt_tokens
            .saturating_sub(cached_prompt_tokens)
            .saturating_sub(cache_write_subset),
        cache_read: cached_prompt_tokens.saturating_add(cache_read_tokens),
        cache_write: cache_creation_tokens.saturating_add(cache_write_subset),
    }
}

pub(crate) fn metric_model_label<'a>(snap: &AisixSnapshot, model_name: &'a str) -> Cow<'a, str> {
    if snap.models.get_by_name(model_name).is_some() {
        return Cow::Borrowed(model_name);
    }
    match crate::model_resolve::wildcard_row_name(snap, model_name) {
        Some(row) => Cow::Owned(row),
        None => Cow::Borrowed(UNRESOLVED_MODEL_LABEL),
    }
}

/// Fixed non-model surface labels the emit chokepoint must pass through
/// byte-identical: rewriting any of these would silently split every
/// series that carries it (the `unknown` note below) and merge distinct
/// surfaces into the `unresolved` bucket. Also an O(1) short-circuit so
/// the tunnel hot paths (passthrough / MCP / A2A / jobs) never pay the
/// wildcard scan.
const FIXED_SURFACE_MODEL_LABELS: &[&str] = &[
    UNRESOLVED_MODEL_LABEL,
    "passthrough",
    "mcp",
    "a2a",
    "unknown",
    "files",
    "batches",
    "fine_tuning",
];

/// The `(model, upstream_model)` label pair for the emit chokepoint —
/// COLLAPSE-ONLY: a name only a wildcard row serves folds to the row's
/// `(display_name, model_name template)` (both halves of a wildcard hit
/// are caller-derived); every other value passes through verbatim.
/// Sentinel fallbacks stay a HANDLER decision (`metric_model_label` on
/// the pre-resolution error paths) — the chokepoint must never rewrite
/// a caller's fixed surface label.
pub(crate) fn metric_model_label_pair<'a>(
    snap: &AisixSnapshot,
    model_name: &'a str,
    upstream_model: &'a str,
) -> (Cow<'a, str>, Cow<'a, str>) {
    if FIXED_SURFACE_MODEL_LABELS.contains(&model_name)
        || snap.models.get_by_name(model_name).is_some()
    {
        return (Cow::Borrowed(model_name), Cow::Borrowed(upstream_model));
    }
    match crate::model_resolve::wildcard_row_identity(snap, model_name) {
        Some((row, template)) => (Cow::Owned(row), Cow::Owned(template)),
        None => (Cow::Borrowed(model_name), Cow::Borrowed(upstream_model)),
    }
}

/// Stamp the five per-PK attribution fields onto an in-progress UsageEvent,
/// sanitising the operator-controlled tag strings (control-char strip + length
/// cap) before they hit the wire. One source of truth for the mapping so the
/// non-chat handlers can't diverge from chat / messages.
pub(crate) fn apply_pk_telemetry(event: &mut UsageEvent, pk: &ResolvedPk<'_>) {
    let tags = pk.telemetry_tags();
    event.provider_kind =
        sanitize_tag(tags.kind.map(|k| k.as_str().to_owned()).unwrap_or_default());
    event.provider_featured = tags.featured;
    event.branded_provider = sanitize_tag(tags.branded_provider.unwrap_or_default());
    event.pk_label = sanitize_tag(tags.pk_label.unwrap_or_default());
    event.byo_label = sanitize_tag(tags.byo_label.unwrap_or_default());
}

/// Stamp the caller-identity attribution fields onto an in-progress
/// UsageEvent: the JWT identity (AISIX-Cloud#564), the org member and team
/// the authenticating key belongs to (AISIX-Cloud#1389), and the request's
/// effective cost-attribution tags. An absent value leaves its field empty,
/// which skip-serializes to wire NULL. One source of truth for the mapping
/// so the handler family can't drift — same rationale as
/// [`apply_pk_telemetry`]. The ids are sanitised like every other
/// externally-influenced tag: the subject is a claim from a verified token,
/// but the identity provider is still not a trusted emitter of control
/// characters or unbounded strings.
///
/// `caller` is one ApiKey row's identity, resolved once against the
/// snapshot the request authenticated on, so every event of a request
/// carries the same member, team and tags, and a name can never sit beside
/// another member's id. A JWT request runs AS a key, so the same identity
/// covers both auth surfaces and the member filter sees every credential a
/// member calls through.
pub(crate) fn apply_caller_identity(
    event: &mut UsageEvent,
    jwt: Option<&std::sync::Arc<crate::auth::JwtIdentity>>,
    caller: &aisix_gateway::CallerIdentity,
) {
    if let Some(name) = caller.api_key_name.as_deref().filter(|n| !n.is_empty()) {
        event.api_key_name = sanitize_tag(name.to_string());
    }
    if let Some(user_id) = caller.user_id.as_deref() {
        event.user_id = sanitize_tag(user_id.to_string());
    }
    if let Some(team_id) = caller.team_id.as_deref() {
        event.team_id = sanitize_tag(team_id.to_string());
    }
    // Request-scoped, like the member pair: resolved once from the
    // snapshot the request authenticated on, so every event of the request
    // carries the same set. Values are operator-typed or identity-provider
    // text bound for line-oriented sinks, so they get the same treatment as
    // the ids above.
    event.tags = caller
        .tags
        .iter()
        .map(|(k, v)| (k.clone(), sanitize_tag(v.clone())))
        .collect();
    if let Some(user_name) = caller.user_name.as_deref() {
        // NOT `sanitize_tag`, unlike the id above. That call exists to keep
        // operator-typed text safe on the cp-api wire and in the log line;
        // `user_name` reaches neither (`#[serde(skip)]`, and
        // `log_provider_call` does not render it). Its only destination is
        // the `user_name` metric label, which the four #890 families
        // already stamp raw off the same ApiKey row — sanitising this one
        // copy would make a long or odd name read differently on
        // `aisix_usage_event*` than on `aisix_llm_*`, and break the
        // cross-family join on the pair for exactly that member. The
        // prometheus exporter escapes exposition characters itself.
        event.user_name = user_name.to_string();
    }
    let Some(jwt) = jwt else {
        return;
    };
    event.jwt_subject = sanitize_tag(jwt.subject.clone());
    event.jwt_provider = sanitize_tag(jwt.provider.clone());
    event.jwt_claim_mapping = sanitize_tag(jwt.claim_mapping.clone().unwrap_or_default());
}

/// The content cap the snapshot's enabled exporters ask for, `None` when none
/// of them runs at `content_mode: full`.
pub(crate) fn content_cap(snap: &AisixSnapshot) -> Option<u32> {
    aisix_obs::content_capture_cap(
        snap.observability_exporters
            .entries()
            .iter()
            .map(|e| &*e.value),
    )
}

/// Hold the request body every failure row of this request reports, in the
/// representation the endpoint's success path captures. `prompt` runs only
/// under a full-content export. Call again once a guardrail has masked the
/// request: the latest note is the one reported.
pub(crate) fn note_failure_request(snap: &AisixSnapshot, prompt: impl FnOnce() -> String) {
    if let Some(cap) = content_cap(snap) {
        crate::attribution::note_failure_request(aisix_obs::CapturedContent::new(
            &prompt(),
            "",
            cap as usize,
        ));
    }
}

/// Take the noted body off this request's failure rows until `chain` has
/// finished with it, when an enforcing input rule applies. The note is the text
/// as the caller sent it: a row written while the rule has not run, because a
/// rate limit turned the request away or the caller hung up during a remote
/// check, must not carry what the rule is there to refuse or mask. A monitored
/// rule changes nothing, so it holds nothing.
///
/// [`crate::quota::reserve_before_costly_check`] calls this, since every route
/// that runs an input chain passes through it before the first await. What ends
/// the hold is [`note_screened_request`], or [`release_failure_request`] on a
/// route that does not rewrite the body. A route that forgets exports no body.
pub(crate) fn hold_failure_request(chain: &dyn aisix_guardrails::Guardrail) {
    if chain.enforces_input() {
        crate::attribution::hold_failure_request();
    }
}

/// [`hold_failure_request`] for a request its chain has not started on, as soon
/// as the caller's key and the model it addressed say which rules apply: a route
/// answering a 400 or a 404 ahead of the chain would otherwise export the text as
/// sent, with no rule run. `model` is the name the request addresses, if it says.
/// One that names no model, or none that exists, still answers to the rules
/// scoped to the environment, the key and its team. A request none of whose rules
/// enforce keeps its body, and the next hold or release takes this one's place.
///
/// Only under a full-content export, so nothing else pays for the lookup.
pub(crate) fn hold_ahead_of_chain(
    state: &ProxyState,
    snap: &AisixSnapshot,
    auth: &crate::auth::AuthenticatedKey,
    model: Option<&str>,
) {
    if content_cap(snap).is_none() {
        return;
    }
    let model_id = model
        .and_then(|name| crate::model_resolve::resolve_model(snap, name))
        .map(|entry| entry.id.clone())
        .unwrap_or_default();
    let chain = state
        .guardrail_index
        .resolve(&aisix_guardrails::RequestContext {
            passthrough_route_id: "",
            model_id: &model_id,
            mcp_server_id: "",
            api_key_id: &auth.entry.id,
            team_id: auth.key().team_id.as_deref(),
        });
    if aisix_guardrails::Guardrail::enforces_input(&chain) {
        crate::attribution::hold_failure_request_ahead_of_chain();
    }
}

/// The input chain has finished with the request and the noted body stands.
pub(crate) fn release_failure_request() {
    crate::attribution::release_failure_request();
}

/// [`note_failure_request`] for the body the input chain has finished with,
/// masks written back: this is what ends [`hold_failure_request`].
pub(crate) fn note_screened_request(snap: &AisixSnapshot, prompt: impl FnOnce() -> String) {
    release_failure_request();
    note_failure_request(snap, prompt);
}

/// Note the copy of a request turned away before `chain` ran that its failure
/// rows report: masked, or no body when that cannot be proved
/// ([`aisix_guardrails::mask_unscreened_request`]). For a route whose rate limit
/// comes ahead of its chain. `prompt` runs only under a full-content export.
pub(crate) fn note_unscreened_request(
    snap: &AisixSnapshot,
    chain: &dyn aisix_guardrails::Guardrail,
    prompt: impl FnOnce() -> String,
) {
    if let Some(cap) = content_cap(snap) {
        if let Some(masked) = aisix_guardrails::mask_unscreened_request(chain, &prompt()) {
            release_failure_request();
            crate::attribution::note_failure_request(aisix_obs::CapturedContent::new(
                &masked,
                "",
                cap as usize,
            ));
        }
    }
}

/// Hold the copy of a request an input guardrail refused that its failure rows
/// report: the body with every hit of the chain's input rules masked, or no
/// body when that cannot be proved ([`aisix_guardrails::mask_refused_request`])
/// or the route found the body unsafe to capture (`capture_safe`: a key a mask
/// cannot rename, say). It wins over any [`note_failure_request`], before or
/// after it, and over what the route captured. `prompt` runs only under a
/// full-content export and is what the success path would capture; nothing
/// here changes the refusal or what an upstream would have received.
pub(crate) fn note_refused_request(
    snap: &AisixSnapshot,
    chain: &dyn aisix_guardrails::Guardrail,
    capture_safe: bool,
    prompt: impl FnOnce() -> String,
) {
    if let Some(cap) = content_cap(snap) {
        let masked = capture_safe
            .then(prompt)
            .and_then(|body| aisix_guardrails::mask_refused_request(chain, &body));
        crate::attribution::note_refused_request(aisix_obs::CapturedContent::new(
            masked.as_deref().unwrap_or_default(),
            "",
            cap as usize,
        ));
    }
}

/// Hold the error body the caller is about to be sent, for the terminal
/// failure row. Only a full-content export reads it.
pub(crate) fn note_client_error(snap: &AisixSnapshot, err: &ProxyError) {
    note_client_error_body(snap, || {
        serde_json::to_string(&err.envelope()).unwrap_or_default()
    });
}

/// [`note_client_error`] for a route that does not answer with the OpenAI
/// envelope.
pub(crate) fn note_client_error_body(snap: &AisixSnapshot, body: impl FnOnce() -> String) {
    if content_cap(snap).is_some() {
        crate::attribution::note_client_error_body(body());
    }
}

/// Whether a served request cost its provider something the row's price has no
/// rate for. A price is per token, so a surface that bills per image, per second
/// or per video and reported no tokens came to nothing here, however it was
/// configured, and a web search is billed on top of the tokens it rode with. A
/// row with no price at all was already counted when it was admitted.
fn costs_what_its_price_cannot_say(
    snap: &AisixSnapshot,
    surface: Surface,
    event: &UsageEvent,
) -> bool {
    const PRICED_IN_OTHER_UNITS: [Surface; 6] = [
        crate::operation::IMAGE_GENERATION,
        crate::operation::IMAGE_EDIT,
        crate::operation::TRANSCRIPTION,
        crate::operation::TRANSLATION,
        crate::operation::SPEECH,
        crate::operation::VIDEO_GENERATION,
    ];
    let no_tokens = event.prompt_tokens == 0 && event.completion_tokens == 0;
    (200..300).contains(&event.status_code)
        && (event.web_search_requests > 0
            || (no_tokens && PRICED_IN_OTHER_UNITS.contains(&surface)))
        && snap
            .models
            .get_by_id(&event.model_id)
            .is_some_and(|row| pricing_index(snap).resolve(&row.value).is_some())
}

/// Whether `event` reports a failure, whatever its status told the caller.
fn is_failure_row(event: &UsageEvent) -> bool {
    event.status_code >= 400 || !event.error_class.is_empty()
}

/// What a failure row exports (AISIX-Cloud#1921): the request body and
/// the attempt's upstream response body, or, for an attempt no upstream
/// answered, the gateway's own error body, except on a `499`, which went to
/// no one. `None` for any other row, whose
/// `content` stands as given.
fn failure_row_content(
    event: &UsageEvent,
    content: Option<&aisix_obs::CapturedContent>,
    upstream_body: Option<&[u8]>,
    terminal: bool,
    cap: usize,
) -> Option<aisix_obs::CapturedContent> {
    if !is_failure_row(event) {
        return None;
    }
    let crate::attribution::FailureNotes {
        request: noted_request,
        client_error: noted_error,
        refused,
        held,
    } = crate::attribution::failure_notes(event.status_code);
    let row = match content {
        // Not what the route captured: nothing masked that.
        _ if refused => {
            noted_request.unwrap_or_else(|| aisix_obs::CapturedContent::new("", "", cap))
        }
        // The chain has not finished with the request, so no request text at all.
        Some(c) if held => aisix_obs::CapturedContent {
            prompt: String::new(),
            prompt_truncated: false,
            truncated: c.response_truncated,
            ..c.clone()
        },
        None if held => aisix_obs::CapturedContent::new("", "", cap),
        Some(c) if !c.prompt.is_empty() => c.clone(),
        given => {
            // A bodyless request's note says nothing; `given` may hold a response.
            let request = noted_request
                .filter(|noted| !noted.prompt.is_empty())
                .or_else(|| given.cloned());
            request.unwrap_or_else(|| aisix_obs::CapturedContent::new("", "", cap))
        }
    };
    if !row.response.is_empty() {
        return Some(row);
    }
    // A stream that failed mid-way ends with no finish reason, which is what
    // the structured output field needs: what it delivered goes here instead.
    let delivered = row
        .output
        .as_ref()
        .and_then(aisix_obs::OutputCapture::parts_json)
        .map(|parts| serde_json::Value::Array(parts).to_string());
    let response = match (delivered, upstream_body.filter(|b| !b.is_empty())) {
        (Some(delivered), _) => delivered,
        (None, Some(body)) => String::from_utf8_lossy(body).into_owned(),
        // The caller left: nothing was sent to report.
        (None, None) if event.status_code == crate::CLIENT_CLOSED_REQUEST => return Some(row),
        (None, None) => noted_error.filter(|_| terminal).unwrap_or_else(|| {
            serde_json::json!({
                "error": { "message": event.error_message, "type": event.error_class }
            })
            .to_string()
        }),
    };
    Some(row.with_response(&response, cap))
}

/// Emit ONE zero-token `UsageEvent` for a FAILED request on a non-chat handler
/// (completions / embeddings / rerank / audio / images / passthrough / jobs /
/// realtime), so the dashboard Logs and budget ledger surface the failure
/// (status and bounded error class) instead of dropping it. Mirrors the #655
/// behavior chat / messages / responses already have: those endpoints emit a
/// zero-token event per failed attempt; the single-attempt non-chat handlers
/// emit one terminal event here.
///
/// `model_id` follows [`crate::attribution::unattempted_model_id`]: the
/// target that settled, else the addressed entry when it is a direct model,
/// else empty (a group that failed before selecting a target).
/// `label` is the usage_sink bucket (#408).
/// `inbound_protocol` must match what the caller's success path emits (e.g.
/// `"passthrough"` for `/passthrough/...`, `"realtime"` for `/v1/realtime`,
/// `"openai"` for the OpenAI-shaped handlers) so Logs protocol filtering sees
/// failures and successes under the same tag.
///
/// A guardrail refusal reaches this function like any other failure, so
/// `guardrail_blocked` is a REQUIRED argument rather than a default: a
/// handler that forgets it produces a 422 the "Guardrail blocks" view
/// cannot find, which reads to an operator as "the gateway logged no
/// guardrail activity at all" (AISIX-Cloud#1428).
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_error_usage_event(
    state: &ProxyState,
    snap: &AisixSnapshot,
    surface: Surface,
    inbound_protocol: &'static str,
    request_id: &str,
    requested_model: &str,
    api_key_id: &str,
    status_code: u16,
    error: EventError,
    // Whether this failure IS a guardrail refusal, from
    // [`ProxyError::is_guardrail_block`]. Every caller but the realtime
    // connect failure — which synthesizes its class without a `ProxyError`
    // — reads it off the error it is reporting.
    guardrail_blocked: bool,
    client: &ClientContext,
    // The request's applied guardrail set, from [`applied_guardrails`].
    // Required for the same reason as `guardrail_blocked`: a refusal whose
    // row names no governing guardrail contradicts its own enforced hit.
    applied: Vec<aisix_core::AppliedGuardrail>,
    // The request's enforced guardrail hits. The failure path is where a
    // `blocked` hit lands — a guardrail refusal IS the error — so the
    // error event is the one that must not drop it. Drained by the caller
    // (`enforced_hits(&audit)`) rather than taken as a handle: the jobs
    // surface accumulates across two separately resolved chains.
    enforced: Vec<aisix_core::GuardrailEnforcedHit>,
    // The request's similarity scores (AISIX-Cloud#1467), drained the same
    // way. A refusal by a `kind: semantic` row lands HERE, so an error
    // event without them would leave the block — the one outcome an
    // operator is certain to look at — as the only execution with no
    // number attached.
    scores: Vec<aisix_core::GuardrailScore>,
    // The request's fail-open bypass tag, drained the same way. A request
    // that went upstream unscreened and THEN failed is still a request
    // that went upstream unscreened, so dropping the tag here would make
    // the field answerable only for the requests that succeeded.
    bypass: String,
) {
    let event = build_error_usage_event(
        inbound_protocol,
        request_id,
        requested_model,
        api_key_id,
        status_code,
        error,
        guardrail_blocked,
        client,
        applied,
        enforced,
        scores,
        bypass,
    );
    // The failed request's own attribution, off the same cell
    // `request_metrics::LastTarget` reads — so the usage-event counters and
    // the request counters agree on which key a failure belongs to
    // (AISIX-Cloud#1317 / #1325).
    let attributed = crate::attribution::current().unwrap_or_default();
    let pk = ResolvedPk::resolve(snap, &attributed.provider_key_id);
    let model = usage_event_model_label(snap, requested_model);
    emit_prepared_usage_event(
        state,
        snap,
        surface,
        event,
        usage_event_labels(&model, &pk),
        client.trace.as_ref(),
    );
}

/// The failure a request's terminal error event names: its
/// [`crate::attempt::error_class`] and
/// [`crate::attempt::gateway_error_message`].
pub(crate) struct EventError {
    class: &'static str,
    message: String,
    /// The model an unknown- or forbidden-model refusal names. Some
    /// surfaces read the caller's `model` only inside dispatch and emit
    /// with an empty `requested_model`; the event records it there, so
    /// the message never carries a name the event does not.
    model: Option<String>,
}

impl EventError {
    pub(crate) fn of(err: &ProxyError) -> Self {
        let model = match err {
            ProxyError::ModelNotFound(model) | ProxyError::ModelForbidden(model) => {
                Some(model.clone())
            }
            _ => None,
        };
        Self {
            class: crate::attempt::error_class(err),
            message: crate::attempt::gateway_error_message(err),
            model,
        }
    }
}

/// Emit one zero-token `UsageEvent` per FAILED attempt a single-shot
/// dispatch recorded (`routing::dispatch_with_failover`) — the #655 shape
/// chat / messages / responses already have, for completions, embeddings,
/// rerank, images, audio and video submits.
///
/// Every event names the target it actually hit (`model_id`, ProviderKey,
/// `attempt_*`) and carries no tokens and no cost: a failed attempt bills
/// nothing, exactly as on chat. The winner, if any, is the handler's own
/// event. When there is none, pass `terminal_last` and the LAST failed
/// attempt in `attempts` becomes the request's terminal event — the one carrying the
/// request-scoped guardrail attribution (`guardrail_blocked`, the enforced
/// hits, scores, `monitor_hits`, `redactions`), which the superseded
/// attempts never repeat. No-op when nothing failed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_failed_attempts(
    state: &ProxyState,
    snap: &AisixSnapshot,
    surface: Surface,
    request_id: &str,
    requested_model: &str,
    api_key_id: &str,
    client: &ClientContext,
    applied_guardrails: &[aisix_core::AppliedGuardrail],
    attempts: &[crate::attempt::AttemptRecord],
    terminal_last: bool,
    guardrail_blocked: bool,
    monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
    redactions: crate::redact::RedactionCounts,
    audit: &GuardrailAudit,
) {
    emit_failed_attempts_as(
        state,
        snap,
        surface,
        "openai",
        request_id,
        requested_model,
        api_key_id,
        client,
        applied_guardrails,
        attempts,
        terminal_last,
        guardrail_blocked,
        monitor_hits,
        redactions,
        audit,
    );
}

/// [`emit_failed_attempts`] for a route whose callers speak a protocol
/// other than OpenAI's — `/v1/messages/count_tokens` records `anthropic`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_failed_attempts_as(
    state: &ProxyState,
    snap: &AisixSnapshot,
    surface: Surface,
    inbound_protocol: &str,
    request_id: &str,
    requested_model: &str,
    api_key_id: &str,
    client: &ClientContext,
    applied_guardrails: &[aisix_core::AppliedGuardrail],
    attempts: &[crate::attempt::AttemptRecord],
    terminal_last: bool,
    guardrail_blocked: bool,
    mut monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
    mut redactions: crate::redact::RedactionCounts,
    audit: &GuardrailAudit,
) {
    let last_failed = attempts.iter().rposition(|a| !a.success);
    let usage_model = usage_event_model_label(snap, requested_model).into_owned();
    for (i, rec) in attempts.iter().enumerate().filter(|(_, a)| !a.success) {
        let terminal = terminal_last && Some(i) == last_failed;
        let mut event = UsageEvent {
            request_id: request_id.to_string(),
            occurred_at: aisix_obs::UsageEvent::occurred_at_now(),
            model_id: rec.target_model_id.clone(),
            api_key_id: api_key_id.to_string(),
            requested_model: requested_model.to_string(),
            upstream_latency_ms: rec.latency_ms,
            status_code: rec.status,
            inbound_protocol: inbound_protocol.to_string(),
            applied_guardrails: applied_guardrails.to_vec(),
            client_source_ip: client.source_ip.clone(),
            client_user_agent: client.user_agent.clone(),
            request_headers: client.request_headers.clone(),
            guardrail_blocked: terminal && guardrail_blocked,
            guardrail_monitor_hits: if terminal {
                std::mem::take(&mut monitor_hits)
            } else {
                Vec::new()
            },
            redacted_entity_counts: if terminal {
                std::mem::take(&mut redactions)
            } else {
                crate::redact::RedactionCounts::new()
            },
            guardrail_enforced_hits: terminal_enforced_hits(terminal, audit),
            guardrail_scores: terminal_guardrail_scores(terminal, audit),
            // Request-scoped, and every attempt of a request that failed
            // open went upstream unscreened — the same rule chat applies.
            guardrail_bypassed_reason: bypass_reason(audit),
            attempt_index: rec.index,
            attempt_kind: rec.kind.to_string(),
            attempt_model: rec.target_model.clone(),
            error_class: rec.error_class.clone(),
            error_message: rec.error_message.clone(),
            ..Default::default()
        };
        let pk = ResolvedPk::resolve(snap, &rec.provider_key_id);
        apply_pk_telemetry(&mut event, &pk);
        apply_caller_identity(&mut event, client.jwt.as_ref(), &client.caller);
        emit_usage(
            state,
            snap,
            surface,
            event,
            usage_event_labels(&usage_model, &pk),
            None,
            client.trace.as_ref(),
            terminal,
            // The record's own network-boundary fact: a rate-limit-refused
            // attempt, or a route's own 501, never reached an upstream.
            rec.dispatched,
        );
    }
}

/// Whether a single-shot request's attempts already produced its terminal
/// usage event: some attempt was made and none succeeded, so
/// [`emit_failed_attempts`] marks the last one terminal and the handler must
/// not emit another.
pub(crate) fn failed_attempts_are_terminal(routing: &crate::attempt::RoutingTelemetry) -> bool {
    !routing.attempts.is_empty() && routing.winner().is_none()
}

/// A single-shot success branch's attempts that [`emit_failed_attempts`]
/// owns. When the request was answered by the route's own 501 for a
/// provider lacking the capability — the LAST record, failed and never
/// dispatched — that refusal is the handler's own terminal event, so it is
/// left out of the slice.
pub(crate) fn split_route_refusal(
    routing: &crate::attempt::RoutingTelemetry,
) -> &[crate::attempt::AttemptRecord] {
    if failed_attempts_are_terminal(routing) {
        &routing.attempts[..routing.attempts.len() - 1]
    } else {
        &routing.attempts
    }
}

/// Stamp the attempt that answered onto a single-shot handler's success
/// event: its index / kind / target, and its own upstream latency — the
/// failed attempts before it emitted their own events, so the whole
/// request's elapsed time would count them twice. `None` (nothing was
/// dispatched) leaves the event as built. The route's own 501 answers through
/// the same event, so a failed record also names the failure on it.
pub(crate) fn apply_winning_attempt(
    event: &mut UsageEvent,
    winner: Option<&crate::attempt::AttemptRecord>,
) {
    if let Some(w) = winner {
        event.attempt_index = w.index;
        event.attempt_kind = w.kind.to_string();
        event.attempt_model = w.target_model.clone();
        event.upstream_latency_ms = w.latency_ms;
        if !w.success {
            event.error_class = w.error_class.clone();
            event.error_message = w.error_message.clone();
        }
    }
}

/// The [`emit_error_usage_event`] event without the emission, for a caller
/// that attributes handler-specific fields (e.g. the passthrough route name)
/// before handing it to [`emit_prepared_usage_event`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_error_usage_event(
    inbound_protocol: &'static str,
    request_id: &str,
    requested_model: &str,
    api_key_id: &str,
    status_code: u16,
    error: EventError,
    // See [`emit_error_usage_event`].
    guardrail_blocked: bool,
    client: &ClientContext,
    applied: Vec<aisix_core::AppliedGuardrail>,
    enforced: Vec<aisix_core::GuardrailEnforcedHit>,
    scores: Vec<aisix_core::GuardrailScore>,
    // See [`emit_error_usage_event`].
    bypass: String,
) -> UsageEvent {
    let requested_model = match error.model {
        Some(model) if requested_model.is_empty() => model,
        _ => requested_model.to_string(),
    };
    let mut event = UsageEvent {
        request_id: request_id.to_string(),
        occurred_at: aisix_obs::UsageEvent::occurred_at_now(),
        api_key_id: api_key_id.to_string(),
        model_id: crate::attribution::unattempted_model_id(&requested_model),
        requested_model,
        status_code,
        inbound_protocol: inbound_protocol.to_string(),
        error_class: error.class.to_string(),
        error_message: error.message,
        client_source_ip: client.source_ip.clone(),
        client_user_agent: client.user_agent.clone(),
        request_headers: client.request_headers.clone(),
        guardrail_blocked,
        applied_guardrails: applied,
        guardrail_enforced_hits: enforced,
        guardrail_scores: scores,
        guardrail_bypassed_reason: bypass,
        ..Default::default()
    };
    apply_caller_identity(&mut event, client.jwt.as_ref(), &client.caller);
    event
}

/// The `model` label for the usage-event counters (AISIX-Cloud#1317).
///
/// `unknown` when the request carried no model at all — the MCP, A2A and
/// passthrough tunnels, and every path that failed before resolution —
/// otherwise collapsed to the configured set exactly like the request
/// families do, because `UsageEvent::requested_model` is caller-controlled
/// text and would otherwise mint one series per made-up name (#451).
pub(crate) fn usage_event_model_label<'a>(
    snap: &AisixSnapshot,
    requested_model: &'a str,
) -> Cow<'a, str> {
    if requested_model.is_empty() {
        Cow::Borrowed(crate::request_metrics::UNKNOWN)
    } else {
        metric_model_label(snap, requested_model)
    }
}

/// Pair that label with the ProviderKey the event is attributed to.
///
/// Takes the resolved key rather than an id for the same reason
/// `request_metrics::Upstream` does: the readable name and the upstream
/// protocol can only come off the row its id names.
pub(crate) fn usage_event_labels<'a>(
    model: &'a str,
    pk: &'a ResolvedPk<'_>,
) -> UsageEventLabels<'a> {
    let labels = pk.labels();
    UsageEventLabels {
        model,
        // `ResolvedPk` reports an unresolved id verbatim, including the
        // empty string the pre-dispatch paths pass. An empty label value
        // would sit next to `unknown` as a second "nothing resolved"
        // series, so collapse it here.
        provider_key_id: if labels.id().is_empty() {
            UNKNOWN_PK
        } else {
            labels.id()
        },
        provider_key_name: labels.name(),
        // Both overwritten by `UsageSink::try_emit` from the event's own
        // member pair (AISIX-Cloud#1389, #1455) — one place, so no handler
        // in this family can build a label set that disagrees with the row.
        user_id: "unknown",
        user_name: "unknown",
        // Same `PkLabels` the request families read (AISIX-Cloud#1403), so
        // `aisix_usage_events_emitted_total` joins with them on
        // `upstream_protocol` instead of dropping out of the aggregation.
        upstream_protocol: labels.protocol(),
    }
}

pub(crate) fn emit_prepared_usage_event(
    state: &ProxyState,
    snap: &AisixSnapshot,
    surface: Surface,
    mut event: UsageEvent,
    labels: UsageEventLabels<'_>,
    trace: Option<&std::sync::Arc<aisix_obs::RequestTraceBundle>>,
) {
    // It speaks for no attempt of its own: after the failed attempts of a
    // single-shot failover whose winner then failed, it follows them.
    event.attempt_index = event
        .attempt_index
        .max(crate::attribution::next_attempt_index());
    // An error event is the request's terminal emission. Its
    // builder leaves `upstream_latency_ms` at 0, so no upstream span is
    // derivable either way — `dispatched: false` states the common case
    // (most error events never reached a provider).
    emit_usage(
        state, snap, surface, event, labels, None, trace, true, false,
    );
}

/// Fill `requested_model_id` from the request's attribution cell, where
/// `resolve_model` recorded the entry the caller's name resolved to. Read
/// here, at the one point every usage event passes through, so every
/// attempt, ensemble member and judge event of a group request carries the
/// group's id beside its own target's `model_id`, and no family can miss it.
///
/// Only taken when the event's `requested_model` is the name that resolved:
/// a name the request never resolved (an unknown model, the passthrough
/// namespace's body `model`) borrows no entry's id. An emitter that runs
/// outside the request scope (the realtime session end, the batch poll)
/// sets the field itself.
fn stamp_requested_model_id(event: &mut UsageEvent) {
    if !event.requested_model_id.is_empty() || event.requested_model.is_empty() {
        return;
    }
    if let Some(id) = crate::attribution::requested_model_id(&event.requested_model) {
        event.requested_model_id = id;
    }
}

/// Stamp the fields the control plane bills a row by, here at the one point
/// every row passes through, so no family's failure, cache-hit or streamed
/// row can lack what its success row carries.
///
/// `event_id` is a random UUID v4 per row, unless the emitter set one. It
/// is never derived from `request_id`: the caller may choose that, and
/// every attempt of a request shares it. The tier the caller asked for is
/// read off the request's cell, where its handler noted it, and bounded
/// like the tier the upstream reports. The upstream identity is
/// [`apply_upstream_identity`] over the ProviderKey this row's labels name
/// and the Model its `model_id` names, so it keys on the DISPATCHED target,
/// never the addressed entry: a direct or embedding model names itself; a
/// routing or semantic attempt and an ensemble member or judge row name
/// the target they called; an ensemble's aggregate and post-panel failure
/// rows, whose `model_id` is the ensemble, name none; a wildcard row
/// addressed directly names the concrete upstream model it filled in, and
/// none when reached through a routing model.
fn stamp_billing_fields(snap: &AisixSnapshot, event: &mut UsageEvent, provider_key_id: &str) {
    if event.event_id.is_empty() {
        event.event_id = uuid::Uuid::new_v4().to_string();
    }
    event.requested_service_tier = event
        .requested_service_tier
        .take()
        .or_else(crate::attribution::requested_service_tier)
        .map(sanitize_tag)
        .filter(|t| !t.is_empty());
    let model = snap.models.get_by_id(&event.model_id);
    apply_upstream_identity(event, provider_key_id, model.as_deref().map(|e| &*e.value));
}

/// The most characters a usage row keeps of any one text a caller or an
/// upstream chose.
const MAX_OUTSIDE_TEXT_CHARS: usize = 512;

/// The most characters a usage row keeps of its `error_message`: the cap
/// every typed error is already rendered under, applied again here for a
/// message built some other way.
const MAX_ERROR_MESSAGE_CHARS: usize = 2048;

/// Bound the text a caller or an upstream chose before the row leaves: control
/// characters stripped and at most [`MAX_OUTSIDE_TEXT_CHARS`] characters kept.
///
/// A row is delivered in a batch with other tenants' rows, and the control
/// plane refuses a batch whose body is too large, permanently. Nothing else
/// bounds a caller's `model` (a request body may be any size), so without this
/// one request could make its row large enough to sink every row it travels
/// with. The text that is already bounded where it is written (`api_key_name`,
/// the tiers, `error_class`) is left to that bound; text within the bound is
/// unchanged.
pub(crate) fn bound_outside_text(event: &mut UsageEvent) {
    for text in [
        &mut event.requested_model,
        &mut event.client_user_agent,
        &mut event.client_identity,
        &mut event.jwt_subject,
        &mut event.mcp_server_name,
        &mut event.mcp_tool_name,
        &mut event.a2a_method,
        &mut event.a2a_task_id,
        &mut event.a2a_context_id,
        &mut event.provider_model_version,
        &mut event.finish_reason,
    ]
    .into_iter()
    // A wildcard row's upstream model is filled in from the caller's name.
    .chain(event.upstream_model.as_mut())
    {
        if text.len() > MAX_OUTSIDE_TEXT_CHARS || text.chars().any(char::is_control) {
            *text = text
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_OUTSIDE_TEXT_CHARS)
                .collect();
        }
    }
    if event.error_message.len() > MAX_ERROR_MESSAGE_CHARS {
        event.error_message = event
            .error_message
            .chars()
            .take(MAX_ERROR_MESSAGE_CHARS)
            .collect();
    }
}

/// Name the upstream credential and model a row's call used: the
/// ProviderKey `provider_key_id` names, and the `provider` and upstream
/// model of `model` when that model calls an upstream itself. A routing,
/// semantic or ensemble model has neither of its own, and the
/// [`UNKNOWN_PK`] an unresolved label carries names no key.
///
/// Composites are excluded by kind rather than left to their empty
/// `provider` and `model_name`: an ensemble's own id is on its aggregate
/// row, and a composite's name may hold a literal `*`, which
/// [`crate::span_facts::request_model`] would read as a wildcard template.
pub(crate) fn apply_upstream_identity(
    event: &mut UsageEvent,
    provider_key_id: &str,
    model: Option<&aisix_core::Model>,
) {
    event.provider_key_id = (!provider_key_id.is_empty() && provider_key_id != UNKNOWN_PK)
        .then(|| provider_key_id.to_string());
    let model = model.filter(|m| !(m.is_routing() || m.is_ensemble() || m.is_semantic()));
    let upstream_model = model.and_then(|m| crate::span_facts::request_model(m, event));
    event.upstream_provider = model.and_then(|m| m.provider.clone());
    event.upstream_model = upstream_model;
}

/// THE emission chokepoint (AISIX-Cloud#1279): every usage event leaves
/// through here, so the CP telemetry leg and the exporter fan-out cannot
/// drift, and the trace snapshot is taken in exactly one place.
///
/// A usage event is the observability record of a request, not a billing
/// line: every request that reaches dispatch emits one, at zero tokens when
/// the upstream reported none or nothing was billable. Never skip it for
/// lack of usage. The one exception is by design: polling a video job
/// (`GET /v1/videos/:id`) and retrieving its content emit none.
///
/// `trace` is the request's bundle (`ClientContext::trace`); `terminal`
/// says whether this event ends the request — the terminal event carries
/// the SERVER + logical spans (ending the SERVER span at NOW, the real
/// body EOF/drop), a non-terminal one carries its own attempt span alone.
/// It is also the wire's `is_final_attempt`, so an emitter owes exactly one
/// `true` per request, on the last row it writes.
/// `dispatched` is the caller's statement that this event describes work
/// that actually reached an upstream — a cache hit and a pre-dispatch
/// error pass `false`, which zeroes the event's `upstream_*` latencies and
/// keeps an upstream CLIENT span from being fabricated. The event's public
/// `trace_id` is stamped here so the CP row and the exported spans always
/// agree.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_usage(
    state: &ProxyState,
    snap: &AisixSnapshot,
    surface: Surface,
    mut event: UsageEvent,
    labels: UsageEventLabels<'_>,
    content: Option<&aisix_obs::CapturedContent>,
    trace: Option<&std::sync::Arc<aisix_obs::RequestTraceBundle>>,
    terminal: bool,
    dispatched: bool,
) {
    // Stamped here rather than at each construction site: this is the one
    // point every usage event passes through, so the operation cannot be
    // present on a family's success path and missing from its error or
    // streaming one (AISIX-Cloud#1461).
    event.operation = surface.operation.to_string();
    event.is_final_attempt = terminal;
    stamp_requested_model_id(&mut event);
    stamp_billing_fields(snap, &mut event, labels.provider_key_id);
    // After the name has looked up its entry, and before the access log and
    // every consumer read the row.
    bound_outside_text(&mut event);
    // Upstream-supplied text bound for line-oriented stores: the same
    // control-character strip and cap as the provider's response id.
    event.service_tier = event
        .service_tier
        .take()
        .map(crate::chat::sanitize_tag)
        .filter(|t| !t.is_empty());
    // Same reasoning, for the other shape of a caller walking away
    // (AISIX-Cloud#1571). A stream the consumer abandoned is reported as
    // 499 by six different families, each through its own emit helper and
    // none of them naming the outcome; a head-phase cancel names it
    // explicitly and would otherwise be the only 499 an operator could
    // filter on. Filling it only when the class is still empty keeps this
    // from overwriting a more specific one, and leaves the head-phase
    // event's own message alone.
    stamp_client_disconnect(
        event.status_code,
        &mut event.error_class,
        &mut event.error_message,
    );
    // A route names its own failures; this is for the one that did not.
    if is_failure_row(&event) && event.error_message.trim().is_empty() {
        event.error_message = aisix_obs::request_failed_message(event.status_code);
    }
    let mut upstream_error_body = None;
    if dispatched {
        let response = crate::attribution::take_attempt_upstream_response(event.attempt_index);
        upstream_error_body = response.error_body;
        // A surface that runs detached from the request scope (`/v1/realtime`)
        // read its own off the upgrade response.
        if event.upstream_response_headers.is_empty() {
            event.upstream_response_headers = response.headers;
        }
        // The response object's id when it had one; otherwise — a failed
        // attempt, a token count, an embedding, an image, a Bedrock answer —
        // the provider's request-id header, which is what its support traces
        // the call by.
        if event.provider_request_id.is_empty() {
            event.provider_request_id = sanitize_provider_response_id(&response.request_id);
        }
    } else {
        // The `upstream_*` latencies time one upstream attempt. An event that
        // reached no upstream has none, whatever duration its builder had at
        // hand (a cache hit's or a refusal's handler time).
        event.upstream_latency_ms = 0;
        event.upstream_ttft_ms = 0;
    }
    // The request's access-log line, for the families that deferred it to
    // their stream (AISIX-Cloud#1571). Here, and after the stamping above,
    // so the line and the event cannot disagree about the outcome: one
    // status, one error class, one message, whichever of a stream's three
    // endings this is. A request that wrote its line inline — everything
    // non-streamed — parked nothing, and this is a no-op for it.
    if terminal {
        crate::attribution::note_terminal_event(
            &event,
            // Only an upstream's own id: a cache hit's stored one is not this
            // request's call.
            if dispatched {
                &event.provider_request_id
            } else {
                ""
            },
        );
        crate::attribution::emit_deferred_access_log(&event);
    }
    // The `upstream` side of the request's end-to-end latency is the
    // terminal attempt's own upstream duration — this figure, so the metric
    // and the row cannot disagree — and only when that attempt reached an
    // upstream at all.
    if terminal && dispatched && costs_what_its_price_cannot_say(snap, surface, &event) {
        crate::quota::note_spend_a_ceiling_cannot_see(state, snap, &event);
    }
    if terminal && dispatched {
        crate::attribution::note_terminal_upstream(std::time::Duration::from_millis(u64::from(
            event.upstream_latency_ms,
        )));
    }
    // Request-level guardrail blocks are recorded from the terminal event,
    // not from an individual timed execution. Some fail-closed paths (for
    // example a streamed-output buffer overflow) reject before a guardrail
    // member runs, while retries and ensembles can emit several non-terminal
    // events for one request.
    if terminal && event.guardrail_blocked {
        state.metrics.record_guardrail_blocked_request();
    }
    // Gateway-initiated semantic embeddings are child work of this request,
    // never attempts for the model the caller addressed. Attach their bounded
    // ledger only to the one terminal parent event: a retry can emit
    // non-terminal attempt rows, and a stream's terminal emitter may run much
    // later, but neither may duplicate or replace the parent's
    // token/cost/model fields.
    if terminal {
        attach_gateway_embedding_audit(&mut event);
    }
    stamp_latency_breakdown(&mut event, terminal);
    // A Responses result carries no finish reason of its own; it is derived
    // from what the caller received, native or bridged alike.
    if terminal && surface.operation == crate::operation::RESPONSES.operation {
        if let Some(finish) = crate::attribution::take_responses_finish() {
            event.finish_reason = finish.to_string();
        }
    }
    let emission = trace.map(|bundle| {
        event.trace_id = bundle.trace_id_hex();
        bundle.emission(
            terminal,
            event.attempt_index,
            event.upstream_latency_ms,
            dispatched,
        )
    });
    // Tell the request's own cell that an event has gone out, so a cancel
    // landing in the moments after cannot double it (AISIX-Cloud#1571). A
    // no-op outside a request scope, which is where the detached stream
    // emitters and the cancel guard itself both run.
    crate::attribution::note_usage_emitted(terminal, event.attempt_index);
    let provider_key_id = labels.provider_key_id;
    state
        .usage_sink
        .try_emit(surface.handler, event.clone(), labels);
    let exporters = live_exporters(state, snap);
    if exporters.is_empty() {
        return;
    }
    // A streamed response's parts were read off the frames the caller got;
    // they belong to the event that ends the request.
    let streamed;
    let content = match content {
        Some(c) if terminal && c.output.is_none() => match crate::attribution::take_stream_output()
        {
            Some(output) => {
                streamed = c.clone().with_output(Some(output));
                Some(&streamed)
            }
            None => Some(c),
        },
        other => other,
    };
    let failure_content = aisix_obs::content_capture_cap(exporters.iter().map(|e| &*e.value))
        .and_then(|cap| {
            failure_row_content(
                &event,
                content,
                upstream_error_body.as_deref(),
                terminal,
                cap as usize,
            )
        });
    let content = failure_content.as_ref().or(content);
    // Export-only span facts: the request-scoped ones the bundle holds, and
    // this event's own upstream, read off the request's snapshot. Only the
    // OTLP encoder reads them, so nothing is resolved without one.
    let facts = exporters
        .iter()
        .any(|e| e.value.enabled && matches!(e.value.kind, aisix_core::ExporterKind::OtlpHttp(_)))
        .then(|| {
            let mut upstream = crate::span_facts::upstream_facts(snap, &event, provider_key_id);
            // An event that never reached its upstream (a cache hit, a
            // refusal) names the model it addressed but no server it called.
            if !dispatched {
                if let Some(u) = upstream.as_mut() {
                    u.server_address.clear();
                    u.server_port = None;
                }
            }
            std::sync::Arc::new(match trace {
                Some(bundle) => bundle.span_facts(upstream),
                None => aisix_obs::SpanFacts {
                    upstream,
                    ..Default::default()
                },
            })
        });
    state.otlp_fan_out.fan_out(
        &event,
        content,
        emission.as_ref(),
        facts.as_ref(),
        exporters.generation(),
        exporters.iter().map(|e| &*e.value),
    );
}

/// `gateway_overhead_ms` on the event that carries `downstream_latency_ms`,
/// and `upstream_tpot_ms` on a successful streamed attempt. Here, at the
/// chokepoint, so no family can carry the latency it reports without them.
///
/// The overhead's subtrahend is the request's upstream wait, measured from
/// monotonic timestamps at the attempt boundaries and the winner's first
/// streamed frame ([`crate::attribution::upstream_wait`]); its window is the
/// same one `downstream_latency_ms` reports.
fn stamp_latency_breakdown(event: &mut UsageEvent, terminal: bool) {
    if terminal && event.downstream_latency_ms > 0 {
        if let Some(waited) = crate::attribution::upstream_wait() {
            let window = std::time::Duration::from_millis(u64::from(event.downstream_latency_ms));
            let overhead = window.saturating_sub(waited).as_millis();
            event.gateway_overhead_ms = Some(u32::try_from(overhead).unwrap_or(u32::MAX));
        }
    }
    let output_tokens = event.genai_output_tokens();
    let succeeded = (200..300).contains(&event.status_code) && event.error_class.is_empty();
    if succeeded && event.upstream_ttft_ms > 0 && output_tokens >= 2 {
        let generating = event
            .upstream_latency_ms
            .saturating_sub(event.upstream_ttft_ms);
        event.upstream_tpot_ms = Some(f64::from(generating) / f64::from(output_tokens - 1));
    }
}

/// Name a `499` that nothing else named: a stream the caller abandoned.
/// [`emit_usage`] applies it to every event; a surface that writes its
/// access-log line before handing the event over applies it first, so the
/// line carries the same class.
pub(crate) fn stamp_client_disconnect(status: u16, class: &mut String, message: &mut String) {
    if status == crate::CLIENT_CLOSED_REQUEST && class.is_empty() {
        *class = crate::CLIENT_DISCONNECTED_KIND.to_string();
        *message = crate::cancel::CANCELLED_MID_STREAM.to_string();
    }
}

/// Move the bounded gateway-embedding child audit to the terminal parent
/// event. Kept beside the emission chokepoint so no request family can attach
/// an unbounded or duplicated child trail on an intermediate attempt.
fn attach_gateway_embedding_audit(event: &mut UsageEvent) {
    let audit = crate::attribution::take_gateway_embedding_audit();
    event.gateway_embedding_calls = audit.calls;
    event.gateway_embedding_calls_dropped = audit.dropped;
}

#[cfg(test)]
mod tests {
    /// 每个请求收多少钱，最终都落在这个函数把 usage 拆成三个桶的方式上。
    /// 此前它一条直接测试都没有——有测试的是 `InputTokens` 上那两个没有
    /// 任何调用方的构造器，而它们的算术与这里并不一致。测试在给一个不
    /// 运行的实现背书。
    ///
    /// 两种上游对 `prompt_tokens` 含义的分歧是这里唯一的难点，两边都靠
    /// 上游文档定死：
    /// - OpenAI：`prompt_tokens_details.cached_tokens` 是 `prompt_tokens`
    ///   的子集，所以要减掉，否则缓存那一半按新鲜价和缓存价各收一次。
    /// - Anthropic：`cache_read_input_tokens` / `cache_creation_input_tokens`
    ///   与 `input_tokens` 不相交，所以不能减，减了就把已经计费的 token
    ///   当成没有。
    /// A refused request's row reports the masked copy held for it or no body:
    /// never the note taken before the chain ran, nor what the route captured.
    /// A refused RESPONSE leaves the request's own note standing.
    #[tokio::test]
    #[allow(clippy::field_reassign_with_default)] // A literal here would read as an emitter to the scans below.
    async fn a_refused_request_row_reports_only_its_masked_copy() {
        let cell = std::sync::Arc::new(crate::attribution::RequestAttribution::default());
        crate::attribution::scope(cell, async {
            let mut event = UsageEvent::default();
            event.status_code = 422;
            let event = event;
            let captured = aisix_obs::CapturedContent::new("route-captured", "", 1000);
            crate::attribution::note_failure_request(aisix_obs::CapturedContent::new(
                "raw-note", "", 1000,
            ));
            let prompt = |content| {
                failure_row_content(&event, content, None, true, 1000)
                    .map(|row| row.prompt)
                    .unwrap_or_default()
            };
            assert_eq!(prompt(None), "raw-note");

            let _ = crate::error::guardrail_block_error("response", None, None);
            let _ = crate::error::guardrail_block_error("tool result", None, None);
            assert_eq!(prompt(None), "raw-note", "the request itself passed");

            let _ = crate::error::guardrail_block_error("request", None, None);
            assert_eq!(prompt(None), "", "a refused request with no masked copy");
            assert_eq!(
                prompt(Some(&captured)),
                "",
                "nothing masked what the route captured"
            );

            crate::attribution::note_refused_request(aisix_obs::CapturedContent::new(
                "masked-copy",
                "",
                1000,
            ));
            assert_eq!(prompt(None), "masked-copy");
            assert_eq!(prompt(Some(&captured)), "masked-copy");

            // A later plain note does not take its place.
            crate::attribution::note_failure_request(aisix_obs::CapturedContent::new(
                "later-note",
                "",
                1000,
            ));
            assert_eq!(prompt(None), "masked-copy");
        })
        .await;
    }

    /// While an enforcing input rule has not finished with a request, its row
    /// reports no request text: not the note taken before the chain ran, not a
    /// note the route's error arm takes meanwhile, and not what the route
    /// captured. What the route captured of the response stays.
    #[tokio::test]
    #[allow(clippy::field_reassign_with_default)] // A literal here would read as an emitter to the scans below.
    async fn a_held_request_row_reports_no_request_text_until_the_chain_has_finished() {
        let cell = std::sync::Arc::new(crate::attribution::RequestAttribution::default());
        crate::attribution::scope(cell, async {
            let mut event = UsageEvent::default();
            event.status_code = 429;
            let event = event;
            let row = |content| {
                failure_row_content(&event, content, None, true, 1000)
                    .unwrap_or_else(|| aisix_obs::CapturedContent::new("", "", 1000))
            };
            let captured = aisix_obs::CapturedContent::new("route-captured", "answer", 1000);
            crate::attribution::note_failure_request(aisix_obs::CapturedContent::new(
                "raw-note", "", 1000,
            ));
            assert_eq!(row(None).prompt, "raw-note");

            crate::attribution::hold_failure_request();
            assert_eq!(row(None).prompt, "");
            let held = row(Some(&captured));
            assert_eq!(held.prompt, "");
            assert_eq!(held.response, "answer");
            crate::attribution::note_failure_request(aisix_obs::CapturedContent::new(
                "raw-error-arm-note",
                "",
                1000,
            ));
            assert_eq!(row(None).prompt, "", "a plain note does not end the hold");

            // The cancel path reads the same answer.
            let mut body = Some(String::new());
            crate::attribution::with_cancel_for_test(|c| {
                body = c.failure_body().map(|b| b.prompt.clone());
            });
            assert_eq!(body, None);

            release_failure_request();
            assert_eq!(row(None).prompt, "raw-error-arm-note");
        })
        .await;
    }

    /// Ahead of the chain a row reports no request text on any status but a 403,
    /// which keeps its body, and the chain starting makes that exception end: a
    /// 403 once it has is held like everything else.
    #[tokio::test]
    #[allow(clippy::field_reassign_with_default)] // A literal here would read as an emitter to the scans below.
    async fn a_row_ahead_of_the_chain_reports_no_request_text_but_a_403_keeps_it() {
        let cell = std::sync::Arc::new(crate::attribution::RequestAttribution::default());
        crate::attribution::scope(cell, async {
            let row = |status| {
                let mut event = UsageEvent::default();
                event.status_code = status;
                let event = event;
                failure_row_content(&event, None, None, true, 1000)
                    .unwrap_or_else(|| aisix_obs::CapturedContent::new("", "", 1000))
                    .prompt
            };
            crate::attribution::note_failure_request(aisix_obs::CapturedContent::new(
                "raw-note", "", 1000,
            ));
            crate::attribution::hold_failure_request_ahead_of_chain();
            for status in [400, 404, 429, 500, 501, crate::CLIENT_CLOSED_REQUEST] {
                assert_eq!(row(status), "", "{status}");
            }
            assert_eq!(row(403), "raw-note");

            // A hang-up reads the same answer.
            let mut body = Some(String::new());
            crate::attribution::with_cancel_for_test(|c| {
                body = c.failure_body().map(|b| b.prompt.clone());
            });
            assert_eq!(body, None);

            crate::attribution::hold_failure_request();
            assert_eq!(row(403), "");

            release_failure_request();
            for status in [400, 403, 404] {
                assert_eq!(row(status), "raw-note", "{status}");
            }
        })
        .await;
    }

    /// The hold comes on where every input chain passes, whether or not the
    /// reservation goes through, and only for a chain that enforces.
    #[tokio::test]
    async fn the_reservation_ahead_of_a_check_holds_the_body_for_an_enforcing_chain() {
        struct Row(bool);
        #[async_trait::async_trait]
        impl aisix_guardrails::Guardrail for Row {
            fn name(&self) -> &'static str {
                "row"
            }
            fn enforces_input(&self) -> bool {
                self.0
            }
        }
        for (enforcing, held) in [(true, true), (false, false)] {
            let cell = std::sync::Arc::new(crate::attribution::RequestAttribution::default());
            crate::attribution::scope(cell, async {
                let chain =
                    aisix_guardrails::GuardrailChain::new(vec![
                        std::sync::Arc::new(Row(enforcing))
                            as std::sync::Arc<dyn aisix_guardrails::Guardrail>,
                    ]);
                let refused = crate::quota::reserve_before_costly_check(&chain, async {
                    Err(ProxyError::InvalidRequest("over the limit".into()))
                })
                .await;
                assert!(refused.is_err());
                assert_eq!(crate::attribution::failure_notes(429).held, held);
            })
            .await;
        }
    }

    #[test]
    fn pricing_buckets_follow_each_upstream_vocabulary() {
        // OpenAI 形状：cached 是子集。
        let openai = input_tokens_for_pricing(1_000, 800, 0, 0, 0);
        assert_eq!(openai.uncached, 200, "子集口径下必须减掉 cached");
        assert_eq!(openai.cache_read, 800);
        assert_eq!(openai.cache_write, 0);
        assert_eq!(
            openai.total(),
            1_000,
            "三个桶加起来必须还是上游报的 prompt 总数",
        );

        // Anthropic 形状：三个计数器互不相交（数值取自
        // aisix-provider-anthropic 的流式夹具）。
        let anthropic = input_tokens_for_pricing(37, 0, 0, 9, 4);
        assert_eq!(
            anthropic.uncached, 37,
            "不相交口径下减掉缓存 token 会让已计费的 token 免费",
        );
        assert_eq!(anthropic.cache_read, 9);
        assert_eq!(anthropic.cache_write, 4);
        assert_eq!(anthropic.total(), 50, "37 + 9 + 4");
    }

    /// `cache_creation_tokens` 只有一个来源：Anthropic 的
    /// `cache_creation_input_tokens`（openai 侧的 wire 硬置 0），而它与
    /// `input_tokens` 不相交。所以它绝不能从 prompt 里减掉——那正是一个
    /// 早期的构造器做过的事。
    #[test]
    fn cache_writes_are_never_subtracted_from_the_prompt() {
        let got = input_tokens_for_pricing(1_000, 0, 0, 0, 300);
        assert_eq!(
            got.uncached, 1_000,
            "cache_write 被从 prompt 里减掉了 —— 那是另一种上游口径，本仓库没有这种来源",
        );
        assert_eq!(got.cache_write, 300);
    }

    /// 上游报出比自己 prompt 总数还大的 cached 数（见过），减法必须饱和，
    /// 不能回绕成一笔天文数字的账单。
    #[test]
    fn an_impossible_cached_count_cannot_wrap_the_fresh_bucket() {
        let got = input_tokens_for_pricing(100, 999, 0, 0, 0);
        assert_eq!(got.uncached, 0);
        assert_eq!(got.cache_read, 999);
    }

    /// OpenAI 形状的缓存写是 `prompt_tokens` 的子集：要从新鲜输入里挪到写入桶，
    /// 否则一个配了 `cache_write_per_1k` 的模型按普通输入价收，少收的正是这个
    /// 字段文档警告的那部分。三个桶加起来仍是上游报的 prompt 总数。
    #[test]
    fn an_openai_shape_cache_write_is_priced_at_the_write_rate() {
        let got = input_tokens_for_pricing(1_000, 100, 700, 0, 0);
        assert_eq!(got.uncached, 200);
        assert_eq!(got.cache_read, 100);
        assert_eq!(got.cache_write, 700);
        assert_eq!(got.total(), 1_000);

        let cost = aisix_core::ModelCost {
            input_per_1k: 10.0,
            output_per_1k: 0.0,
            cached_input_per_1k: Some(1.0),
            cache_write_per_1k: Some(12.5),
        };
        // 200*10 + 100*1 + 700*12.5 = 10850，再除以 1000。
        assert!((cost.calculate_with_cache(got, 0) - 10.85).abs() < 1e-9);
    }

    /// USD→micro-USD 必须四舍五入。单次调用常在 1e-4 美元量级（这里是
    /// 0.00012345 USD = 123.45 micro-USD），一律截断会让每一次调用都少记
    /// 不到 1 micro-USD——单看无所谓，跨几十万次调用就是系统性偏低。
    #[test]
    fn spend_rounds_rather_than_truncating() {
        use super::spend_micro_usd;
        assert_eq!(spend_micro_usd(0.000_123_45), 123);
        assert_eq!(spend_micro_usd(0.000_123_65), 124);
        // 比 1 micro-USD 还小的一次调用记成 1 而不是 0。
        assert_eq!(spend_micro_usd(0.000_000_6), 1);
        assert_eq!(spend_micro_usd(0.0), 0);
        // 未定价模型返回 0.0，对花费桶的贡献就是 0。
        assert_eq!(spend_micro_usd(1.0), 1_000_000);
    }

    /// 负价与 NaN 都不能变成一个巨大的正数记进桶里。
    #[test]
    fn spend_clamps_negative_and_nan_to_zero() {
        use super::spend_micro_usd;
        assert_eq!(spend_micro_usd(-1.0), 0);
        assert_eq!(spend_micro_usd(f64::NAN), 0);
    }

    /// `Model.cost` had two live readers (`least_cost` ranking, and the jobs
    /// and realtime emits) and was ignored by every main LLM endpoint, which
    /// emitted a hard-coded `0.0`. A deployment with no control plane to
    /// recompute spend therefore saw no cost at all.
    #[test]
    fn request_cost_uses_the_dispatched_row_price() {
        use aisix_core::resource::ResourceEntry;
        use aisix_core::snapshot::ResourceTable;

        let table = ResourceTable::default();
        let priced: aisix_core::Model = serde_json::from_value(serde_json::json!({
            "display_name": "priced",
            "provider": "openai",
            "model_name": "gpt-4o",
            "provider_key_id": "pk-1",
            "cost": { "input_per_1k": 2.5, "output_per_1k": 10.0 },
        }))
        .unwrap();
        table.insert(ResourceEntry::new("m-priced", priced, 1));
        let unpriced: aisix_core::Model = serde_json::from_value(serde_json::json!({
            "display_name": "unpriced",
            "provider": "openai",
            "model_name": "gpt-4o-mini",
            "provider_key_id": "pk-1",
        }))
        .unwrap();
        table.insert(ResourceEntry::new("m-unpriced", unpriced, 1));
        let snap = AisixSnapshot {
            models: table,
            ..Default::default()
        };

        // 1000 prompt + 500 completion = 2.5 + 5.0
        let cost = request_cost_usd(
            &snap,
            "m-priced",
            aisix_core::InputTokens::uncached_only(1_000),
            500,
        );
        assert!((cost - 7.5).abs() < 1e-9, "got {cost}");

        assert_eq!(
            request_cost_usd(
                &snap,
                "m-unpriced",
                aisix_core::InputTokens::uncached_only(1_000),
                500
            ),
            0.0,
            "no configured price means the control plane owns the number"
        );
        assert_eq!(
            request_cost_usd(
                &snap,
                "m-missing",
                aisix_core::InputTokens::uncached_only(1_000),
                500
            ),
            0.0,
            "a row that no longer resolves must not panic or guess"
        );
    }

    /// Wrapping a token count understates usage, and the upstream that
    /// reported it is not always a trusted party (`api_base` points wherever
    /// the operator configured). Saturate rather than wrap.
    #[test]
    fn token_count_saturates_instead_of_wrapping() {
        assert_eq!(token_count(0), 0);
        assert_eq!(token_count(1_000), 1_000);
        assert_eq!(token_count(u32::MAX as u64), u32::MAX);
        assert_eq!(
            token_count(u32::MAX as u64 + 1),
            u32::MAX,
            "one past the ceiling must clamp, not become 0"
        );
        assert_eq!(token_count(u64::MAX), u32::MAX);
    }
    use super::*;

    /// Routes name their failures, so no request drives this fallback. It is
    /// the chokepoint's own guarantee for one that forgets, read at the sink
    /// every consumer of the event reads.
    #[tokio::test]
    #[allow(clippy::field_reassign_with_default)]
    async fn emit_usage_names_a_failure_row_nothing_named() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let state = crate::ProxyState::new(
            aisix_core::snapshot::SnapshotHandle::new(AisixSnapshot::new()),
            std::sync::Arc::new(aisix_gateway::Hub::new()),
            &aisix_core::ProxyConfig {
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
            },
        )
        .without_cache()
        .with_usage_sink(aisix_obs::UsageSink::new(tx));
        let snap = state.snapshot.load();
        let mut emit = |status_code: u16, class: &str, message: &str| {
            let mut event = UsageEvent::default();
            event.status_code = status_code;
            event.error_class = class.to_string();
            event.error_message = message.to_string();
            emit_usage(
                &state,
                &snap,
                crate::operation::CHAT,
                event,
                usage_event_labels("m", &ResolvedPk::unresolved()),
                None,
                None,
                true,
                false,
            );
            rx.try_recv()
                .expect("the event reaches the sink")
                .error_message
        };
        assert_eq!(emit(500, "", ""), "request failed with status 500");
        assert_eq!(
            emit(422, "some_class", ""),
            "request failed with status 422"
        );
        assert_eq!(
            emit(200, "some_class", ""),
            "request failed with status 200"
        );
        assert_eq!(emit(500, "", " "), "request failed with status 500");
        assert_eq!(emit(502, "some_class", "named"), "named");
        assert_eq!(emit(200, "", ""), "");
    }

    /// A state whose usage sink the test reads, serving `snap`.
    fn state_reading_rows(
        snap: AisixSnapshot,
    ) -> (crate::ProxyState, tokio::sync::mpsc::Receiver<UsageEvent>) {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let state = crate::ProxyState::new(
            aisix_core::snapshot::SnapshotHandle::new(snap),
            std::sync::Arc::new(aisix_gateway::Hub::new()),
            &aisix_core::ProxyConfig {
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
            },
        )
        .without_cache()
        .with_usage_sink(aisix_obs::UsageSink::new(tx));
        (state, rx)
    }

    /// Hand `event` to the chokepoint as an attempt made with the
    /// ProviderKey `pk_id` names, and read back the row the sink received.
    fn emitted_row(
        state: &crate::ProxyState,
        rx: &mut tokio::sync::mpsc::Receiver<UsageEvent>,
        event: UsageEvent,
        pk_id: &str,
    ) -> UsageEvent {
        let snap = state.snapshot.load();
        let pk = ResolvedPk::resolve(&snap, pk_id);
        emit_usage(
            state,
            &snap,
            crate::operation::CHAT,
            event,
            usage_event_labels("m", &pk),
            None,
            None,
            true,
            true,
        );
        rx.try_recv().expect("the row reaches the sink")
    }

    #[allow(clippy::field_reassign_with_default)] // A literal here would read as an emitter to the scans below.
    fn with_request_id(request_id: &str) -> UsageEvent {
        let mut event = UsageEvent::default();
        event.request_id = request_id.to_string();
        event
    }

    #[tokio::test]
    async fn every_usage_row_carries_a_unique_event_id() {
        let (state, mut rx) = state_reading_rows(AisixSnapshot::new());
        let a = emitted_row(&state, &mut rx, UsageEvent::default(), "");
        let b = emitted_row(&state, &mut rx, UsageEvent::default(), "");
        assert!(!a.event_id.is_empty());
        assert_ne!(a.event_id, b.event_id);
        let id = uuid::Uuid::parse_str(&a.event_id).expect("an event id is a UUID");
        assert_eq!(id.get_version_num(), 4, "minted at random, not derived");
    }

    /// `request_id` is the caller's to choose and several rows share it, so
    /// it can never be what tells two rows apart.
    #[tokio::test]
    async fn a_caller_supplied_request_id_never_becomes_the_event_id() {
        let (state, mut rx) = state_reading_rows(AisixSnapshot::new());
        let row = emitted_row(&state, &mut rx, with_request_id("1"), "");
        assert_eq!(row.request_id, "1");
        assert_ne!(row.event_id, "1");
        let again = emitted_row(&state, &mut rx, with_request_id("1"), "");
        assert_ne!(again.event_id, row.event_id, "two rows, one request id");
    }

    #[tokio::test]
    #[allow(clippy::field_reassign_with_default)] // A literal here would read as an emitter to the scans below.
    async fn the_row_names_the_provider_key_and_upstream_model() {
        let snap = snap_with_pk("pk-1", "");
        let model: aisix_core::Model = serde_json::from_value(serde_json::json!({
            "display_name": "gpt-4o",
            "provider": "openai",
            "model_name": "gpt-4o-2024-08-06",
            "provider_key_id": PK_ID,
        }))
        .unwrap();
        snap.models.insert(ResourceEntry::new("m-1", model, 1));
        let (state, mut rx) = state_reading_rows(snap);
        let mut event = UsageEvent::default();
        event.model_id = "m-1".to_string();
        let row = emitted_row(&state, &mut rx, event, PK_ID);
        assert_eq!(row.provider_key_id.as_deref(), Some(PK_ID));
        assert_eq!(row.upstream_provider.as_deref(), Some("openai"));
        assert_eq!(row.upstream_model.as_deref(), Some("gpt-4o-2024-08-06"));
    }

    /// A wildcard row fills `upstream_model` in from the name the caller
    /// sent, so it is the caller's text as much as `requested_model` is and
    /// gets the same bound. Unbounded, a megabyte-long name served by a
    /// wildcard model would make its row too large to travel, and the
    /// tokens it bought would never reach the control plane.
    #[tokio::test]
    #[allow(clippy::field_reassign_with_default)] // A literal here would read as an emitter to the scans below.
    async fn a_wildcard_rows_upstream_model_is_bounded_like_the_name_it_came_from() {
        let snap = snap_with_pk("pk-1", "");
        let wildcard: aisix_core::Model = serde_json::from_value(serde_json::json!({
            "display_name": "openai/*",
            "provider": "openai",
            "model_name": "*",
            "provider_key_id": PK_ID,
        }))
        .unwrap();
        snap.models
            .insert(ResourceEntry::new("m-star", wildcard, 1));
        let (state, mut rx) = state_reading_rows(snap);
        let mut event = UsageEvent::default();
        event.model_id = "m-star".to_string();
        event.requested_model = format!("openai/\u{0}{}", "x".repeat(1 << 20));
        let row = emitted_row(&state, &mut rx, event, PK_ID);
        let upstream = row
            .upstream_model
            .expect("a wildcard row names its upstream model");
        assert!(
            upstream.chars().count() <= 512 && !upstream.chars().any(char::is_control),
            "{} chars",
            upstream.chars().count()
        );
        assert!(upstream.starts_with("xxx"), "{:?}", &upstream[..8]);
        assert!(serde_json::to_vec(&row.requested_model).unwrap().len() < 1024);
    }

    /// A row that reached no ProviderKey, or names an entry that is not an
    /// upstream of its own, carries none rather than a guess.
    ///
    /// The ensemble is the composite whose own id a row can carry (its
    /// aggregate row), and its name holds a literal `*`, which nothing
    /// forbids. Addressed by that exact name, it would read as a wildcard
    /// template filled with `*` if composites were not excluded.
    #[tokio::test]
    #[allow(clippy::field_reassign_with_default)] // A literal here would read as an emitter to the scans below.
    async fn a_row_that_resolved_no_upstream_names_none() {
        let snap = AisixSnapshot::new();
        let ensemble: aisix_core::Model = serde_json::from_value(serde_json::json!({
            "display_name": "council-*",
            "ensemble": {"panel": [{"model": "a"}], "judge": {"model": "j"}},
        }))
        .unwrap();
        snap.models.insert(ResourceEntry::new("e-1", ensemble, 1));
        let (state, mut rx) = state_reading_rows(snap);
        for model_id in ["", "e-1", "missing"] {
            let mut event = UsageEvent::default();
            event.model_id = model_id.to_string();
            event.requested_model = "council-*".to_string();
            let row = emitted_row(&state, &mut rx, event, "");
            assert_eq!(row.provider_key_id, None, "{model_id:?}");
            assert_eq!(row.upstream_provider, None, "{model_id:?}");
            assert_eq!(row.upstream_model, None, "{model_id:?}");
        }
    }

    /// The tier is the caller's own text, bound for the control plane's
    /// store: control characters are stripped and it is capped at 256
    /// characters, like the tier the upstream reports.
    #[tokio::test]
    async fn the_requested_service_tier_is_bounded_before_it_leaves() {
        let (state, mut rx) = state_reading_rows(AisixSnapshot::new());
        let cell = std::sync::Arc::new(crate::attribution::RequestAttribution::default());
        let asked = format!("fl\u{7}ex{}", "x".repeat(300));
        crate::attribution::scope(cell, async {
            crate::attribution::note_requested_service_tier(Some(&asked));
            let row = emitted_row(&state, &mut rx, UsageEvent::default(), "");
            let expected = format!("flex{}", "x".repeat(252));
            assert_eq!(
                row.requested_service_tier.as_deref(),
                Some(expected.as_str())
            );
        })
        .await;
    }

    /// Text a caller or an upstream chose is bound on the row: control
    /// characters stripped and at most 512 characters kept, so one request
    /// cannot make its row large enough for the control plane to refuse the
    /// batch it travels in. The model name an unknown-model request carries
    /// is the one a caller can make as long as its body.
    #[tokio::test]
    #[allow(clippy::field_reassign_with_default)] // A literal here would read as an emitter to the scans below.
    async fn caller_and_upstream_text_is_bounded_before_it_leaves() {
        let (state, mut rx) = state_reading_rows(AisixSnapshot::new());
        let huge = format!("m\u{0}\n{}", "x".repeat(1 << 20));
        let bounded = format!("m{}", "x".repeat(511));
        let mut event = UsageEvent::default();
        event.requested_model = huge.clone();
        event.client_user_agent = huge.clone();
        event.client_identity = huge.clone();
        event.jwt_subject = huge.clone();
        event.mcp_server_name = huge.clone();
        event.mcp_tool_name = huge.clone();
        event.a2a_method = huge.clone();
        event.a2a_task_id = huge.clone();
        event.a2a_context_id = huge.clone();
        event.provider_model_version = huge.clone();
        event.finish_reason = huge.clone();
        event.error_class = "upstream_status".into();
        event.error_message = "e".repeat(1 << 20);
        let row = emitted_row(&state, &mut rx, event, "");
        for (field, value) in [
            ("requested_model", &row.requested_model),
            ("client_user_agent", &row.client_user_agent),
            ("client_identity", &row.client_identity),
            ("jwt_subject", &row.jwt_subject),
            ("mcp_server_name", &row.mcp_server_name),
            ("mcp_tool_name", &row.mcp_tool_name),
            ("a2a_method", &row.a2a_method),
            ("a2a_task_id", &row.a2a_task_id),
            ("a2a_context_id", &row.a2a_context_id),
            ("provider_model_version", &row.provider_model_version),
            ("finish_reason", &row.finish_reason),
        ] {
            assert!(
                value == &bounded,
                "{field}: {} chars, starting {:?}",
                value.chars().count(),
                value.chars().take(8).collect::<String>(),
            );
        }
        assert_eq!(row.error_message.chars().count(), 2048);
        let wire = serde_json::to_vec(&row).unwrap();
        assert!(wire.len() < 16 * 1024, "a row of {} bytes", wire.len());

        // Text within the bound is left exactly as it was.
        let mut event = UsageEvent::default();
        event.requested_model = "gpt-4o".into();
        event.client_user_agent = "codex-cli/1.2".into();
        let row = emitted_row(&state, &mut rx, event, "");
        assert_eq!(row.requested_model, "gpt-4o");
        assert_eq!(row.client_user_agent, "codex-cli/1.2");
    }

    /// Every row of a request carries the tier its body asked for, the
    /// superseded attempts as well as the final one; a request that asked
    /// for none reports none.
    #[tokio::test]
    async fn every_row_of_a_request_reports_the_service_tier_it_asked_for() {
        let (state, mut rx) = state_reading_rows(AisixSnapshot::new());
        let asked = std::sync::Arc::new(crate::attribution::RequestAttribution::default());
        crate::attribution::scope(asked, async {
            crate::attribution::note_requested_service_tier(Some("flex"));
            for _ in 0..2 {
                let row = emitted_row(&state, &mut rx, UsageEvent::default(), "");
                assert_eq!(row.requested_service_tier.as_deref(), Some("flex"));
            }
        })
        .await;
        let silent = std::sync::Arc::new(crate::attribution::RequestAttribution::default());
        crate::attribution::scope(silent, async {
            crate::attribution::note_requested_service_tier(None);
            let row = emitted_row(&state, &mut rx, UsageEvent::default(), "");
            assert_eq!(row.requested_service_tier, None);
        })
        .await;
    }

    #[test]
    fn summed_upstream_total_stands_only_while_every_counting_report_carried_one() {
        let with = |n: u64| serde_json::json!({ "input_tokens": 1, "total_tokens": n });
        let without = serde_json::json!({ "input_tokens": 1 });

        let mut all = ReportedTotalSum::default();
        all.add(&with(4), true);
        all.add(&with(6), true);
        // A cancelled response's empty usage counted nothing.
        all.add(&serde_json::Value::Null, false);
        assert_eq!(all.recorded(), 10);

        let mut gap = ReportedTotalSum::default();
        gap.add(&with(4), true);
        gap.add(&without, true);
        gap.add(&with(6), true);
        assert_eq!(gap.recorded(), 0);
    }

    #[tokio::test]
    async fn terminal_event_receives_the_bounded_gateway_embedding_audit_once() {
        let parent = std::sync::Arc::new(crate::attribution::RequestAttribution::default());
        crate::attribution::scope(parent, async {
            for index in 0..=crate::attribution::MAX_GATEWAY_EMBEDDING_CALLS {
                crate::attribution::note_gateway_embedding_call(aisix_obs::GatewayEmbeddingCall {
                    count: 1,
                    purpose: aisix_obs::GatewayEmbeddingPurpose::SemanticCache,
                    embedding_model_id: format!("embed-{index}"),
                    prompt_tokens: index as u32,
                    total_tokens: index as u32,
                    usage_source: aisix_obs::GatewayEmbeddingUsageSource::Reported,
                    latency_ms: index as u32,
                    outcome: aisix_obs::GatewayEmbeddingOutcome::Succeeded,
                });
            }

            let mut terminal = UsageEvent::default();
            attach_gateway_embedding_audit(&mut terminal);
            assert_eq!(
                terminal.gateway_embedding_calls.len(),
                crate::attribution::MAX_GATEWAY_EMBEDDING_CALLS,
            );
            assert_eq!(
                terminal.gateway_embedding_calls[0].embedding_model_id,
                "embed-0"
            );
            assert_eq!(terminal.gateway_embedding_calls_dropped, 1);

            let mut duplicate_terminal = UsageEvent::default();
            attach_gateway_embedding_audit(&mut duplicate_terminal);
            assert!(duplicate_terminal.gateway_embedding_calls.is_empty());
            assert_eq!(duplicate_terminal.gateway_embedding_calls_dropped, 0);
        })
        .await;
    }

    #[tokio::test]
    async fn terminal_usage_event_replaces_an_unsafe_embedding_model_id_with_unknown() {
        let parent = std::sync::Arc::new(crate::attribution::RequestAttribution::default());
        crate::attribution::scope(parent, async {
            crate::attribution::note_gateway_embedding_call(aisix_obs::GatewayEmbeddingCall {
                count: 1,
                purpose: aisix_obs::GatewayEmbeddingPurpose::SemanticRoute,
                embedding_model_id: "界".repeat(100),
                prompt_tokens: 1,
                total_tokens: 1,
                usage_source: aisix_obs::GatewayEmbeddingUsageSource::Reported,
                latency_ms: 1,
                outcome: aisix_obs::GatewayEmbeddingOutcome::Succeeded,
            });

            let mut terminal = UsageEvent::default();
            attach_gateway_embedding_audit(&mut terminal);
            let model_id = &terminal.gateway_embedding_calls[0].embedding_model_id;
            assert_eq!(model_id, "unknown");
        })
        .await;
    }

    #[test]
    fn live_exporters_uses_current_membership_when_the_request_started_empty() {
        let frozen = AisixSnapshot::new();
        let handle = aisix_core::snapshot::SnapshotHandle::new(frozen.clone());
        let state = ProxyState::new(
            handle.clone(),
            std::sync::Arc::new(aisix_gateway::Hub::new()),
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
        );
        let current = AisixSnapshot::new();
        let exporter = serde_json::from_value(serde_json::json!({
            "name": "new-exporter",
            "kind": "otlp_http",
            "endpoint": "https://collector.example/v1/traces"
        }))
        .unwrap();
        current
            .observability_exporters
            .insert(aisix_core::resource::ResourceEntry::new(
                "exporter-id",
                exporter,
                2,
            ));
        handle.store(current);

        let exporters = live_exporters(&state, &frozen);
        assert_eq!(exporters.len(), 1);
        assert_eq!(exporters[0].value.name, "new-exporter");
    }

    #[test]
    fn metric_model_label_three_outcomes() {
        use aisix_core::resource::ResourceEntry;
        use aisix_core::snapshot::ResourceTable;
        let table = ResourceTable::default();
        let wildcard: aisix_core::Model = serde_json::from_value(serde_json::json!({
            "display_name": "openai/*",
            "provider": "openai",
            "model_name": "*",
            "provider_key_id": "pk-1",
        }))
        .unwrap();
        table.insert(ResourceEntry::new("m-star", wildcard, 1));
        let snap = AisixSnapshot {
            models: table,
            ..Default::default()
        };
        // Exact configured name labels as itself (the wildcard row's own
        // name IS an exact name).
        assert_eq!(metric_model_label(&snap, "openai/*"), "openai/*");
        // A caller-minted suffix a wildcard row serves labels as the ROW,
        // not the unbounded concrete string (#451 class on the success
        // path).
        assert_eq!(metric_model_label(&snap, "openai/gpt-4o"), "openai/*");
        assert_eq!(
            metric_model_label(&snap, "openai/anything-else"),
            "openai/*"
        );
        // Unservable text stays the sentinel.
        assert_eq!(
            metric_model_label(&snap, "no-such/model"),
            UNRESOLVED_MODEL_LABEL
        );

        // The emit-chokepoint pair is COLLAPSE-ONLY: a wildcard hit folds
        // BOTH halves to the row's configured identities…
        let (m, u) = metric_model_label_pair(&snap, "openai/gpt-4o", "gpt-4o");
        assert_eq!((m.as_ref(), u.as_ref()), ("openai/*", "*"));
        // …an exact hit passes both through…
        let (m, u) = metric_model_label_pair(&snap, "openai/*", "somemodel");
        assert_eq!((m.as_ref(), u.as_ref()), ("openai/*", "somemodel"));
        // …and fixed surface sentinels and unresolvable values are NEVER
        // rewritten (rewriting `mcp`/`passthrough`/`unknown` would split
        // every series that carries them).
        for sentinel in ["passthrough", "mcp", "a2a", "unknown"] {
            let (m, u) = metric_model_label_pair(&snap, sentinel, "x");
            assert_eq!((m.as_ref(), u.as_ref()), (sentinel, "x"));
        }
        let (m, u) = metric_model_label_pair(&snap, "no-such/model", "raw-upstream");
        assert_eq!((m.as_ref(), u.as_ref()), ("no-such/model", "raw-upstream"));
    }

    /// #1289: the id is upstream-controlled and reaches a log line
    /// and the control plane's `control plane_usage_events`. A newline in it would break the
    /// one-record-per-line shape every log consumer relies on, and an
    /// unbounded id would ride every line for the request. Both entry points
    /// must normalise, because producers use whichever fits their decode:
    /// JSON bodies take the `Value` form, bridge/stream structs the `&str`.
    #[test]
    fn both_entry_points_strip_control_chars_and_cap_length() {
        let injected = "chatcmpl-1\nfake-log-line status=200";
        assert_eq!(
            sanitize_provider_response_id(injected),
            "chatcmpl-1fake-log-line status=200",
        );
        assert_eq!(
            provider_response_id(&serde_json::json!({ "id": injected })),
            "chatcmpl-1fake-log-line status=200",
        );

        let long = "x".repeat(1000);
        assert_eq!(sanitize_provider_response_id(&long).chars().count(), 256);
        assert_eq!(
            provider_response_id(&serde_json::json!({ "id": long }))
                .chars()
                .count(),
            256,
        );
    }

    /// A response with no id — the normal case on several endpoints — must
    /// stay empty rather than becoming a placeholder, and a non-string `id`
    /// must not be coerced into one.
    #[test]
    fn absent_or_non_string_id_stays_empty() {
        assert_eq!(sanitize_provider_response_id(""), "");
        assert_eq!(provider_response_id(&serde_json::json!({})), "");
        assert_eq!(provider_response_id(&serde_json::json!({ "id": 42 })), "");
        assert_eq!(provider_response_id(&serde_json::json!({ "id": null })), "");
    }

    const PK_ID: &str = "22222222-2222-2222-2222-222222222222";

    fn snap_with_pk(display_name: &str, tags: &str) -> AisixSnapshot {
        let json = format!(
            r#"{{"display_name":{},"secret":"sk-up","api_base":"http://up","provider":"openai","adapter":"openai"{}}}"#,
            serde_json::to_string(display_name).unwrap(),
            tags,
        );
        let pk: ProviderKey = serde_json::from_str(&json).unwrap();
        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(ResourceEntry::new(PK_ID, pk, 1));
        snap
    }

    /// The `provider_key_id` / `provider_key_name` pair is a metric label
    /// set — a changed fallback would silently split every series that
    /// carries it. Both halves report `"unknown"` when nothing resolved,
    /// which is what `Upstream::default()` and the pre-dispatch rejection
    /// paths rely on.
    #[test]
    fn unresolved_key_labels_both_halves_unknown() {
        let snap = AisixSnapshot::new();
        for pk in [
            ResolvedPk::unresolved(),
            ResolvedPk::resolve(&snap, ""),
            ResolvedPk::resolve(&snap, UNKNOWN_PK),
            ResolvedPk::resolve(&snap, PK_ID),
        ] {
            assert_eq!(pk.labels().name, "unknown");
            // AISIX-Cloud#1403: same fallback, same reason — a request
            // that reached no key reached no protocol either.
            assert_eq!(pk.labels().protocol, "unknown");
            assert_eq!(pk.telemetry_tags(), aisix_core::TelemetryTags::default());
        }
        assert_eq!(PkLabels::default().id, "unknown");
        assert_eq!(PkLabels::default().name, "unknown");
        assert_eq!(PkLabels::default().protocol, "unknown");
    }

    /// AISIX-Cloud#1403: the protocol is read off the SAME row as the id
    /// and the name, on the one lookup this type exists to make, so the
    /// three can never describe different keys.
    #[test]
    fn resolved_key_carries_its_upstream_protocol() {
        let snap = snap_with_pk("prod-openai", "");
        let pk = ResolvedPk::resolve(&snap, PK_ID);
        assert_eq!(pk.labels().id, PK_ID);
        assert_eq!(pk.labels().protocol, "openai");

        // A key whose vendor has no specialized bridge takes its
        // adapter's wire value.
        let json = r#"{"display_name":"byo-anthropic","secret":"sk","provider":"byo","adapter":"anthropic"}"#;
        let byo: ProviderKey = serde_json::from_str(json).unwrap();
        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(ResourceEntry::new(PK_ID, byo, 1));
        assert_eq!(
            ResolvedPk::resolve(&snap, PK_ID).labels().protocol,
            "anthropic",
            "an open vendor string cannot stand in for the adapter"
        );
    }

    /// …and the usage-event counter's label set is built off that same
    /// resolved key, so `aisix_usage_events_emitted_total` reports the
    /// protocol of the key its `provider_key_id` names.
    ///
    /// It was the one family carrying `inbound_protocol` that had the
    /// value in hand and did not emit it (found in 0.11.0-rc.2 QA), which
    /// left it unjoinable with the families that did:
    /// `sum by (upstream_protocol)` returned an aggregate with this
    /// counter silently missing from it.
    /// A price is per token. What a surface billed in another unit comes to
    /// nothing here, and only a row that has a price can say so: one with none
    /// was counted when it was admitted.
    #[test]
    #[allow(clippy::field_reassign_with_default)] // 字面量会被 UsageEvent 的普查当成产品代码。
    fn what_a_price_cannot_say_is_told_apart_from_what_it_can() {
        let snap = AisixSnapshot::new();
        let row = |cost: serde_json::Value| -> aisix_core::Model {
            serde_json::from_value(serde_json::json!({
                "display_name": "m",
                "provider": "openai",
                "model_name": "m",
                "provider_key_id": "pk",
                "cost": cost,
            }))
            .unwrap()
        };
        let priced = row(serde_json::json!({"input_per_1k": 1.0, "output_per_1k": 1.0}));
        snap.models.insert(aisix_core::resource::ResourceEntry::new(
            "priced", priced, 1,
        ));
        let unpriced = row(serde_json::Value::Null);
        snap.models.insert(aisix_core::resource::ResourceEntry::new(
            "unpriced", unpriced, 1,
        ));
        let event = |model: &str, status: u16, tokens: u32, searches: u32| {
            let mut event = UsageEvent::default();
            event.model_id = model.to_string();
            event.status_code = status;
            event.prompt_tokens = tokens;
            event.web_search_requests = searches;
            event
        };
        let image = crate::operation::IMAGE_GENERATION;
        let chat = crate::operation::CHAT;

        assert!(costs_what_its_price_cannot_say(
            &snap,
            image,
            &event("priced", 200, 0, 0)
        ));
        assert!(costs_what_its_price_cannot_say(
            &snap,
            chat,
            &event("priced", 200, 9, 1)
        ));
        // An image model that does report tokens is priced by them.
        assert!(!costs_what_its_price_cannot_say(
            &snap,
            image,
            &event("priced", 200, 9, 0)
        ));
        assert!(!costs_what_its_price_cannot_say(
            &snap,
            chat,
            &event("priced", 200, 9, 0)
        ));
        assert!(!costs_what_its_price_cannot_say(
            &snap,
            image,
            &event("priced", 500, 0, 0)
        ));
        assert!(!costs_what_its_price_cannot_say(
            &snap,
            image,
            &event("unpriced", 200, 0, 0)
        ));
    }

    #[test]
    fn usage_event_labels_carry_the_resolved_upstream_protocol() {
        let snap = snap_with_pk("prod-openai", "");
        let pk = ResolvedPk::resolve(&snap, PK_ID);
        let labels = usage_event_labels("gpt-4o", &pk);
        assert_eq!(labels.provider_key_id, PK_ID);
        assert_eq!(labels.provider_key_name, "prod-openai");
        // The UPSTREAM's protocol. An Anthropic-protocol caller served by
        // this key must still read `openai` here — the inbound protocol
        // travels on its own label.
        assert_eq!(labels.upstream_protocol, "openai");

        // Nothing resolved: `unknown`, matching the id/name fallback
        // beside it and the value the request families use for a
        // pre-dispatch rejection.
        let unresolved = ResolvedPk::unresolved();
        let labels = usage_event_labels("unknown", &unresolved);
        assert_eq!(labels.provider_key_id, UNKNOWN_PK);
        assert_eq!(labels.upstream_protocol, "unknown");

        // A deleted id keeps reaching the label verbatim, but there is no
        // row left to read a protocol off.
        let empty = AisixSnapshot::new();
        let gone = ResolvedPk::resolve(&empty, "pk-deleted");
        let labels = usage_event_labels("gpt-4o", &gone);
        assert_eq!(labels.provider_key_id, "pk-deleted");
        assert_eq!(labels.upstream_protocol, "unknown");
    }

    /// The id reaches the label verbatim even when it resolves to nothing —
    /// an id the operator deleted mid-request still names which key the
    /// request tried to use, and rewriting it to `"unknown"` would merge
    /// those samples with the never-resolved ones.
    #[test]
    fn unresolvable_id_still_reaches_the_label() {
        let snap = AisixSnapshot::new();
        assert_eq!(
            ResolvedPk::resolve(&snap, "pk-deleted").labels().id,
            "pk-deleted"
        );
        assert_eq!(ResolvedPk::resolve(&snap, "").labels().id, "");
    }

    /// One lookup now feeds the metric label AND the wire attribution tags,
    /// so both have to come off the same row.
    #[test]
    fn one_resolve_serves_both_the_label_and_the_tags() {
        let snap = snap_with_pk(
            "prod-openai",
            r#","telemetry_tags":{"kind":"catalog","featured":true,"branded_provider":"openai","pk_label":"prod"}"#,
        );
        let pk = ResolvedPk::resolve(&snap, PK_ID);
        assert_eq!(pk.labels().id, PK_ID);
        assert_eq!(pk.labels().name, "prod-openai");
        let tags = pk.telemetry_tags();
        assert!(tags.featured);
        assert_eq!(tags.branded_provider.as_deref(), Some("openai"));
        assert_eq!(tags.pk_label.as_deref(), Some("prod"));
    }

    /// `display_name` is operator-controlled and reaches a Prometheus label,
    /// so it keeps the `sanitize_tag` treatment the per-emit lookup applied:
    /// control characters stripped, 256 chars max. A name that sanitises
    /// away entirely falls back to `"unknown"` rather than emitting a blank
    /// label value.
    #[test]
    fn display_name_is_sanitised_and_blank_falls_back() {
        let snap = snap_with_pk("prod\nopenai", "");
        assert_eq!(
            ResolvedPk::resolve(&snap, PK_ID).labels().name,
            "prodopenai"
        );

        let long = "x".repeat(1000);
        let snap = snap_with_pk(&long, "");
        let pk = ResolvedPk::resolve(&snap, PK_ID);
        assert_eq!(pk.labels().name.chars().count(), 256);

        let snap = snap_with_pk("\u{1}\u{2}", "");
        assert_eq!(ResolvedPk::resolve(&snap, PK_ID).labels().name, "unknown");
    }

    /// Every `UsageEvent` this crate builds must set
    /// `guardrail_bypassed_reason` and `applied_guardrails`, and an emitter
    /// that genuinely has no chain behind it must say so where it is
    /// written.
    ///
    /// Both fields default to empty, so an emitter that forgets one reports
    /// "nothing was bypassed" for a request that went upstream unscreened,
    /// or "no guardrail governed this" for one a guardrail refused — and no
    /// behavioural test can see the omission, because an unset field and an
    /// unguarded request produce the same row. That is exactly how the
    /// bypass reason spent its whole life written on
    /// `/v1/chat/completions` and nowhere else, and how every
    /// single-attempt failure row lost its applied set (api7/aisix#1030).
    ///
    /// A parse of the source rather than a list of emitters, for the reason
    /// `guardrail_coverage` parses the router instead of listing routes: a
    /// hand-written list nobody updates agrees with itself forever, and the
    /// emitter family here has sixteen members that keep gaining a
    /// seventeenth.
    #[test]
    fn every_usage_event_this_crate_builds_answers_the_guardrail_questions() {
        /// Written inside an emitter that has no guardrail chain behind it
        /// at all, followed by why.
        const EXEMPT: &str = "NO-GUARDRAIL-CHAIN:";

        let literals = usage_event_literals();
        let blocks = literals.len();
        let mut missing = Vec::new();
        for (site, block) in &literals {
            if block.contains(EXEMPT) {
                continue;
            }
            for field in ["guardrail_bypassed_reason", "applied_guardrails"] {
                if !sets_field(block, field) {
                    missing.push(format!("{site} ({field})"));
                }
            }
        }

        // The parse must actually find the emitters, not silently yield an
        // empty set that makes the assertion below vacuous.
        // The crate's real count. A floor rather than an equality so
        // adding an emitter does not fail here (the `missing` check
        // already governs a new one) — but losing four to a parse that
        // quietly stopped matching is the drift this exists to catch.
        assert!(
            blocks >= 18,
            "the UsageEvent literal scan found only {blocks} — it has stopped tracking the \
             emitter family",
        );
        assert!(
            missing.is_empty(),
            "these UsageEvent emitters neither set the named field nor carry a \
             `{EXEMPT} <why>` comment saying they have no guardrail chain: {missing:?}\n\
             An unset field reads as an unguarded request, so an emitter that skips it makes \
             the field unusable as a negative answer.",
        );
    }

    /// Every `UsageEvent` this crate builds carries the request's
    /// `request_headers`, so the operator-selected headers reach every
    /// event of every family — success, failure, per-attempt, cancel, and
    /// the after-the-fact batch rows alike. An emitter that forgot it would
    /// export rows silently missing the attribution, which no test of
    /// another family can see.
    #[test]
    fn every_usage_event_this_crate_builds_carries_the_request_headers() {
        let literals = usage_event_literals();
        assert!(
            literals.len() >= 18,
            "the UsageEvent literal scan found only {}",
            literals.len()
        );
        // The cancel emitter's literals spread a base event that sets it.
        let missing: Vec<&str> = literals
            .iter()
            .filter(|(_, block)| {
                !sets_field(block, "request_headers") && !block.contains("..base_event(")
            })
            .map(|(site, _)| site.as_str())
            .collect();
        assert!(
            missing.is_empty(),
            "these UsageEvent literals do not set request_headers: {missing:?}"
        );
    }

    /// Every `UsageEvent` this crate builds carries the caller identity —
    /// the member, the team and the request's effective cost-attribution
    /// tags — so every row of a request (each attempt, the terminal and
    /// cancel rows, the after-the-fact batch rows) is attributed alike. The
    /// fields default to empty, which reads as "no team, no tags" rather
    /// than as a missing stamp, so no behavioural test of another family
    /// can see an emitter that forgot it.
    ///
    /// The stamp is `apply_caller_identity`, called on the built event in
    /// the rest of the block the literal sits in; the cancel emitter's
    /// literals spread a base event that calls it. An emitter with no
    /// authenticated caller behind it says so with a `NO-CALLER-IDENTITY:`
    /// comment.
    #[test]
    fn every_usage_event_this_crate_builds_carries_the_caller_identity() {
        const EXEMPT: &str = "NO-CALLER-IDENTITY:";
        let literals = usage_event_literals_in_context();
        assert!(
            literals.len() >= 18,
            "the UsageEvent literal scan found only {}",
            literals.len()
        );
        let missing: Vec<&str> = literals
            .iter()
            .filter(|(_, block, rest)| {
                !block.contains("..base_event(")
                    && !block.contains(EXEMPT)
                    && !rest.contains("apply_caller_identity(")
            })
            .map(|(site, _, _)| site.as_str())
            .collect();
        assert!(
            missing.is_empty(),
            "these UsageEvent literals are never handed to apply_caller_identity: {missing:?}"
        );
    }

    /// [`usage_event_literals`] with, for each, the rest of the innermost
    /// block the literal sits in — where the event it builds is stamped.
    fn usage_event_literals_in_context() -> Vec<(String, String, String)> {
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = Vec::new();
        for path in crate_rs_files(&src_dir) {
            let src = std::fs::read_to_string(&path).expect("source must read");
            let name = path
                .strip_prefix(&src_dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let code = code_mask(&src);
            for (idx, _) in src.match_indices("UsageEvent {") {
                if !code[idx] {
                    continue;
                }
                let before = src[..idx]
                    .trim_end()
                    .trim_end_matches(|c: char| c.is_alphanumeric() || c == '_' || c == ':')
                    .trim_end();
                if before.ends_with("->") {
                    continue;
                }
                let open = idx + "UsageEvent ".len();
                let block = braced_block(&src, &code, open).expect("balanced literal");
                let after = open + block.len();
                let b = src.as_bytes();
                let mut depth = 0i64;
                let mut end = b.len();
                for i in after..b.len() {
                    if !code[i] {
                        continue;
                    }
                    match b[i] {
                        b'{' => depth += 1,
                        b'}' if depth == 0 => {
                            end = i;
                            break;
                        }
                        b'}' => depth -= 1,
                        _ => {}
                    }
                }
                let line = src[..idx].lines().count();
                out.push((
                    format!("{name}:{line}"),
                    block.to_string(),
                    src[after..end].to_string(),
                ));
            }
        }
        out
    }

    /// Every `UsageEvent { .. }` literal in this crate's source, as
    /// `("<file>:<line>", block)`. A parse of the source rather than a list
    /// of emitters — see the guardrail census above for why.
    fn usage_event_literals() -> Vec<(String, String)> {
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = Vec::new();
        for path in crate_rs_files(&src_dir) {
            let src = std::fs::read_to_string(&path).expect("source must read");
            let name = path
                .strip_prefix(&src_dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let code = code_mask(&src);
            for (idx, _) in src.match_indices("UsageEvent {") {
                // Skip anything that is not Rust code: this file writes the
                // token it searches for in a string literal and in a
                // comment, and both would otherwise be counted as emitters
                // — a check inflating its own coverage.
                if !code[idx] {
                    continue;
                }
                // `-> UsageEvent {` (optionally path-qualified) is a
                // return type followed by a function body, not a literal.
                let before = src[..idx]
                    .trim_end()
                    .trim_end_matches(|c: char| c.is_alphanumeric() || c == '_' || c == ':')
                    .trim_end();
                if before.ends_with("->") {
                    continue;
                }
                let open = idx + "UsageEvent ".len();
                let Some(block) = braced_block(&src, &code, open) else {
                    panic!("{name}: unbalanced UsageEvent literal at byte {idx}");
                };
                let line = src[..idx].lines().count();
                out.push((format!("{name}:{line}"), block.to_string()));
            }
        }
        out
    }

    /// Every `.rs` file under `dir`, sorted. Recursive: `src/` is flat
    /// today, and a refactor that moved a handler into a subdirectory would
    /// otherwise take its emitter out of scope while a scan stayed green.
    fn crate_rs_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let entries = std::fs::read_dir(dir).expect("the crate's own src/ must be readable");
            for path in entries.filter_map(|e| e.ok().map(|e| e.path())) {
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(dir, &mut files);
        files.sort();
        files
    }

    /// Every `occurred_at` this crate stamps goes through
    /// `UsageEvent::occurred_at_now`, so the whole emitter family writes one
    /// format (millisecond RFC 3339, AISIX-Cloud#1368). A hand-rolled
    /// `to_rfc3339_opts` at one site would put a second-precision stamp back
    /// on that surface, and nothing downstream rejects it: it still parses,
    /// it just ties with every other row of the same second.
    #[test]
    fn every_occurred_at_is_stamped_by_the_shared_helper() {
        const HELPER: &str = "occurred_at: aisix_obs::UsageEvent::occurred_at_now()";
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stamped = 0usize;
        let mut hand_rolled = Vec::new();
        for path in crate_rs_files(&src_dir) {
            let src = std::fs::read_to_string(&path).expect("source must read");
            let code = code_mask(&src);
            for (idx, _) in src.match_indices("occurred_at:") {
                if !code[idx] {
                    continue;
                }
                if src[idx..].starts_with(HELPER) {
                    stamped += 1;
                } else {
                    let line = src[..idx].lines().count();
                    hand_rolled.push(format!("{}:{line}", path.display()));
                }
            }
        }
        // The crate's real count is twenty; a floor keeps the scan from
        // passing vacuously if it stops matching.
        assert!(
            stamped >= 20,
            "the occurred_at scan found only {stamped} stamps"
        );
        assert!(
            hand_rolled.is_empty(),
            "these sites set occurred_at without UsageEvent::occurred_at_now: {hand_rolled:?}",
        );
    }

    /// A byte mask over `src` marking the bytes that are Rust CODE —
    /// false inside string literals (raw ones included), char literals,
    /// line comments and nestable block comments.
    ///
    /// A real lexer pass rather than a per-line heuristic. The heuristic
    /// this replaces judged a match by whether its line prefix held a `//`
    /// or an odd number of quotes, which gets two things wrong that occur
    /// in ordinary code: a literal after a completed string on the same
    /// line (`let u = "https://x"; ... UsageEvent {`) reads as commented
    /// out, and an escaped quote miscounts the parity.
    fn code_mask(src: &str) -> Vec<bool> {
        #[derive(Clone, Copy)]
        enum St {
            Code,
            Str,
            Raw(usize),
            Char,
            Line,
            Block(usize),
        }
        let b = src.as_bytes();
        let mut mask = vec![false; b.len()];
        let mut st = St::Code;
        let mut i = 0;
        while i < b.len() {
            match st {
                St::Code => {
                    mask[i] = true;
                    match b[i] {
                        b'/' if b.get(i + 1) == Some(&b'/') => {
                            st = St::Line;
                            mask[i] = false;
                        }
                        b'/' if b.get(i + 1) == Some(&b'*') => {
                            st = St::Block(1);
                            mask[i] = false;
                            i += 2;
                            continue;
                        }
                        b'"' => st = St::Str,
                        // A char literal is `'x'` or `'\\n'`; anything
                        // else starting with a quote is a LIFETIME
                        // (`&'a str`), and treating one as a literal
                        // swallows every byte up to the next apostrophe.
                        b'\'' if b.get(i + 1) == Some(&b'\\') || b.get(i + 2) == Some(&b'\'') => {
                            st = St::Char
                        }
                        b'r' => {
                            // `r"…"` / `r#"…"#`, but not an identifier
                            // ending in `r`, and not a lifetime.
                            let prev_ident =
                                i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
                            let mut j = i + 1;
                            while b.get(j) == Some(&b'#') {
                                j += 1;
                            }
                            if !prev_ident && b.get(j) == Some(&b'"') {
                                st = St::Raw(j - i - 1);
                                i = j + 1;
                                continue;
                            }
                        }
                        _ => {}
                    }
                }
                St::Str => {
                    if b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == b'"' {
                        st = St::Code;
                    }
                }
                St::Raw(hashes) => {
                    if b[i] == b'"' && b[i + 1..].iter().take(hashes).all(|c| *c == b'#') {
                        st = St::Code;
                        i += hashes + 1;
                        continue;
                    }
                }
                St::Char => {
                    if b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == b'\'' {
                        st = St::Code;
                    }
                }
                St::Line => {
                    if b[i] == b'\n' {
                        st = St::Code;
                    }
                }
                St::Block(depth) => {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        st = St::Block(depth + 1);
                        i += 2;
                        continue;
                    }
                    if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        st = if depth == 1 {
                            St::Code
                        } else {
                            St::Block(depth - 1)
                        };
                        i += 2;
                        continue;
                    }
                }
            }
            i += 1;
        }
        mask
    }

    /// Whether an emitter block ASSIGNS `field`, rather than merely
    /// mentioning it.
    ///
    /// `block.contains(field)` would be satisfied by a comment, and by
    /// `guardrail_bypassed_reason: String::new()` — which is exactly the
    /// shape of a reverted fix, so the check would have been unfalsifiable
    /// in the one dimension it is here to guard.
    fn sets_field(block: &str, field: &str) -> bool {
        let Some(at) = block
            .find(&format!("{field}:"))
            .or_else(|| block.find(&format!("{field},")))
        else {
            return false;
        };
        let rest = &block[at + field.len()..];
        // Field-init shorthand (`guardrail_bypassed_reason,`) takes its
        // value from a binding, which cannot be an inline empty literal.
        let Some(rest) = rest.strip_prefix(':') else {
            return rest.starts_with(',');
        };
        // Values run to the end of their line, except the one that wraps
        // onto continuation lines — whose first line is a receiver, which
        // is non-empty and passes for the right reason.
        let value = rest
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .trim_end_matches(',')
            .trim();
        !matches!(
            value,
            "" | "\"\""
                | "String::new()"
                | "String::default()"
                | "Default::default()"
                | "Vec::new()"
                | "vec![]"
        )
    }

    /// The `{ … }` at `open`, skipping braces the mask says are not code.
    fn braced_block<'a>(src: &'a str, code: &[bool], open: usize) -> Option<&'a str> {
        let b = src.as_bytes();
        assert_eq!(b[open], b'{');
        let mut depth = 0usize;
        for i in open..b.len() {
            if !code[i] {
                continue;
            }
            match b[i] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&src[open..=i]);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// The mask is load-bearing for the guard above, so it is pinned
    /// directly: every one of these would be a false positive or a false
    /// negative under the line-prefix heuristic it replaces.
    #[test]
    fn the_code_mask_excludes_strings_comments_and_raw_strings() {
        let at = |src: &str, needle: &str| {
            let m = code_mask(src);
            m[src.find(needle).expect("needle must appear")]
        };
        assert!(at("let x = 1; UsageEvent {}", "UsageEvent"));
        assert!(!at("let s = \"UsageEvent {\";", "UsageEvent"));
        assert!(!at("// UsageEvent {", "UsageEvent"));
        assert!(!at("/* a /* b */ UsageEvent */", "UsageEvent"));
        assert!(!at("let s = r#\"UsageEvent {\"#;", "UsageEvent"));
        // The two the heuristic got wrong: a completed string earlier on
        // the line, and an escaped quote inside one.
        assert!(at("let u = \"https://x\"; UsageEvent {}", "UsageEvent"));
        assert!(at("let e = \"a\\\"b\"; UsageEvent {}", "UsageEvent"));
        // A char literal must not open a string, and an identifier ending
        // in `r` must not open a raw string.
        assert!(at("let c = '\"'; UsageEvent {}", "UsageEvent"));
        assert!(at("let var = 1; UsageEvent {}", "UsageEvent"));
    }
}
