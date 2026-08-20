//! Pre-dispatch quota gate shared by every LLM endpoint.
//!
//! Applies budget + multi-layer rate limiting:
//! 1. Budget pre-check (the control plane cached decision)
//! 2. API-key inline rate limit (`auth.entry.id`)
//! 3. Model inline rate limit (`model:<name>`) — when the resolved Model has one
//! 4. MCP-server rate limit (`mcp:<api_key_id>:<server>`) — on an MCP
//!    `tools/call`, the key's `mcp_rate_limits` entry for the server the
//!    call targets
//! 5. Policy-based rate limits — looked up from the snapshot's
//!    `rate_limit_policies` table. Classic rows match by scope
//!    (api_key/model/team/member/team_member); `team_member` is a
//!    per-member default for a team: it matches every key in the team
//!    but buckets the counter per `user_id`, so each member gets an
//!    independent identical quota (vs. `team`, one shared bucket).
//!    Conditional rows (#892) match by their `conditions`
//!    tree and bucket by `group_by` — see [`match_policy_layer`] for
//!    the phase split that decides whether a row reserves at the
//!    request gate or per routing target.
//!
//! All layers use AND logic — every layer must pass or the request gets
//! 429. The returned [`MultiReservation`] commits token usage to all
//! layers and releases all concurrency permits on drop.

use aisix_core::models::{
    ConditionInput, GroupByDimension, PolicyScope, PolicyWindow, RateLimitPolicy,
};
use aisix_core::RateLimit;
use aisix_ratelimit::{CounterUnit, MultiReservation};

use crate::auth::AuthenticatedKey;
use crate::error::ProxyError;
use crate::state::ProxyState;

/// Optional model rate-limit info resolved by the caller before enforce.
pub(crate) struct ModelRateLimit {
    pub name: String,
    /// The resolved entry's `display_name` — the bucket identity. Equal
    /// to `name` everywhere except a wildcard-served alias, where every
    /// caller-minted concrete name must land in the wildcard row's ONE
    /// bucket: keying on `name` there let any caller mint fresh
    /// full-size buckets per suffix, unbounded-multiplying the declared
    /// cap (the passthrough gate already keyed on `display_name`).
    pub bucket_name: String,
    pub entry_id: String,
    pub limits: Option<RateLimit>,
    /// The model's `provider` — the value of the `provider` condition
    /// dimension. `None` on routing/ensemble/semantic parents.
    pub provider: Option<String>,
    /// Whether the entry is a virtual parent (routing / ensemble /
    /// semantic): its concrete targets reserve their own model layers
    /// per attempt, so the request gate defers model-property
    /// conditional policies to the per-target phase.
    pub routing_parent: bool,
}

impl ModelRateLimit {
    /// Build from a resolved model entry. Always returns a
    /// `ModelRateLimit` carrying the model identity (name + entry ID)
    /// needed for model-scope policy matching. The inline rate limit
    /// is `None` when the model has no configured limit.
    pub fn from_model(model_name: &str, model_entry_id: &str, model: &aisix_core::Model) -> Self {
        let limits = model
            .rate_limit
            .as_ref()
            .filter(|rl| !rl.is_unrestricted())
            .cloned();
        Self {
            name: model_name.to_owned(),
            bucket_name: model.display_name.clone(),
            entry_id: model_entry_id.to_owned(),
            limits,
            provider: model.provider.clone(),
            routing_parent: model.is_routing() || model.is_ensemble() || model.is_semantic(),
        }
    }
}

/// Identity of the caller-addressed virtual parent (routing group /
/// ensemble / semantic router), forwarded by the dispatch loops into the
/// per-target condition input so `model` / `model_name` leaves match the
/// parent as well as the concrete target (#1267).
#[derive(Clone, Copy)]
pub(crate) struct RoutingParent<'a> {
    /// The parent Model's `display_name` (the alias the caller sent).
    pub name: &'a str,
    /// The parent's resource entry id.
    pub entry_id: &'a str,
}

/// The request's condition-dimension values at this gate point. Model
/// dimensions are absent when no model is resolved (MCP, A2A) — leaves
/// on them evaluate false while OR siblings can still match.
/// `routing_parent` is set only at the per-target gate of a routing
/// dispatch; the request gate passes `None` (there the primary values
/// already ARE the requested entry).
fn condition_input<'a>(
    auth: &'a AuthenticatedKey,
    model_rl: Option<&'a ModelRateLimit>,
    routing_parent: Option<RoutingParent<'a>>,
) -> ConditionInput<'a> {
    ConditionInput {
        team: auth.key().team_id.as_deref(),
        member: auth.key().user_id.as_deref(),
        api_key: Some(&auth.entry.id),
        model: model_rl.map(|m| m.entry_id.as_str()),
        model_name: model_rl.map(|m| m.name.as_str()),
        provider: model_rl.and_then(|m| m.provider.as_deref()),
        routing_parent_model: routing_parent.map(|p| p.entry_id),
        routing_parent_model_name: routing_parent.map(|p| p.name),
    }
}

/// Which scan of the policy table this is. Every policy reserves at
/// exactly one phase per attempt:
///
/// - classic rows: `model` scope rows follow the model (per target on a
///   routing dispatch), every other scope reserves at the request gate;
/// - conditional rows: rows referencing a model property reserve where
///   the concrete model is known — the request gate for a direct
///   dispatch, the per-target gate when the request entry is a
///   routing/ensemble parent (`defer_model_properties`); rows touching
///   no model property reserve once at the request gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PolicyPhase {
    Request { defer_model_properties: bool },
    ModelTarget,
}

/// One policy layer this request must reserve at the current phase.
struct PolicyLayer {
    bucket_key: String,
    limits: RateLimit,
    /// 花费层：与 token 层同一条策略、同一个窗口，但独立的桶和独立的
    /// 计数单位。两层都要过——任一超限即拒，和多层限流的语义一致。
    /// `None` = 该策略没设 `max_spend_micro_usd`，或窗口投影为空
    /// （`second` 窗口，已在 [`classic_rate_limit`] 里报警）。
    spend: Option<SpendLayer>,
}

/// [`PolicyLayer`] 的花费侧。桶键与限额都独立于 token 侧。
struct SpendLayer {
    bucket_key: String,
    limits: RateLimit,
}

/// Decide whether `policy` applies at this phase and build its bucket
/// key + effective limits. `None` = not applicable here (wrong phase,
/// unmatched, suspended is checked by the caller, or a `group_by`
/// dimension the request does not carry).
fn match_policy_layer(
    policy: &RateLimitPolicy,
    policy_entry_id: &str,
    input: &ConditionInput<'_>,
    phase: PolicyPhase,
) -> Option<PolicyLayer> {
    if policy.is_conditional() {
        return match_conditional_layer(policy, policy_entry_id, input, phase);
    }
    // —— classic form ——
    let (Some(scope), Some(scope_ref)) = (policy.scope, policy.scope_ref.as_deref()) else {
        // Load-time validation rejects formless rows; nothing to enforce.
        return None;
    };
    if matches!(phase, PolicyPhase::ModelTarget) && scope != PolicyScope::Model {
        // Request-level scopes were reserved at the request gate; only
        // the model scope follows the target.
        return None;
    }
    let applies = match scope {
        PolicyScope::ApiKey => input.api_key == Some(scope_ref),
        PolicyScope::Model => input.model == Some(scope_ref),
        PolicyScope::Team => input.team == Some(scope_ref),
        PolicyScope::Member => input.member == Some(scope_ref),
        // Per-member default for a team: matches every key whose
        // team_id == scope_ref, but only when the key carries a
        // user_id (the bucket is keyed per member below).
        PolicyScope::TeamMember => input.team == Some(scope_ref) && input.member.is_some(),
    };
    if !applies {
        return None;
    }
    let limits = classic_rate_limit(policy)?;
    // Most scopes share one counter across every key the policy matches
    // (`policy:<scope>:<scope_ref>:<id>`). `team_member` appends the
    // request's `user_id` so each member of the team counts against an
    // independent identical bucket (LiteLLM's `{team_id}:{user_id}`).
    //
    // 花费桶跟着同一个后缀：token 上限在 `team_member` 下是每人一份，
    // 花费桶不拆就会变成全队共用一份预算——两个维度的拆分方式必须一致。
    let member_suffix = match (scope, input.member) {
        (PolicyScope::TeamMember, Some(member)) => format!(":{member}"),
        _ => String::new(),
    };
    let bucket_key = format!("policy:{scope}:{scope_ref}:{policy_entry_id}{member_suffix}");
    let spend = policy.max_spend_micro_usd.and_then(|max_spend| {
        let limits = spend_limits_for(policy.window?, max_spend);
        // second 窗口投影为空，此时不预留（已在 `classic_rate_limit` 报警）。
        (!limits.is_unrestricted()).then(|| SpendLayer {
            bucket_key: format!(
                "{}{member_suffix}",
                spend_bucket_key(scope.as_str(), scope_ref, policy_entry_id)
            ),
            limits,
        })
    });
    // 只设了花费上限的策略也要生效：token 侧无限制不代表整条策略无限制。
    if limits.is_unrestricted() && spend.is_none() {
        return None;
    }
    Some(PolicyLayer {
        bucket_key,
        limits,
        spend,
    })
}

fn match_conditional_layer(
    policy: &RateLimitPolicy,
    policy_entry_id: &str,
    input: &ConditionInput<'_>,
    phase: PolicyPhase,
) -> Option<PolicyLayer> {
    let follows_model = policy.references_model_property();
    let due_here = match phase {
        PolicyPhase::Request {
            defer_model_properties,
        } => !(follows_model && defer_model_properties),
        PolicyPhase::ModelTarget => follows_model,
    };
    if !due_here {
        return None;
    }
    if !aisix_core::models::eval_condition_nodes(
        policy.conditions.as_deref().unwrap_or_default(),
        input,
    ) {
        return None;
    }
    // Bucket: `policy:v2:<policy_id>` plus one `:<dim>=<value>` segment
    // per `group_by` dimension, in canonical order so the declared
    // order never changes the bucket identity. A matched request
    // missing a split dimension is not subject to the policy (mirrors
    // `team_member` only applying to keys carrying a `user_id`).
    let group_by = policy.group_by.as_deref().unwrap_or_default();
    let mut bucket_key = format!("policy:v2:{policy_entry_id}");
    for dim in GroupByDimension::CANONICAL_ORDER {
        if !group_by.contains(&dim) {
            continue;
        }
        let value = input.get_group_by(dim)?;
        bucket_key = format!("{bucket_key}:{dim}={}", escape_bucket_segment(value));
    }
    let limits = policy.limits.clone()?;
    if limits.is_unrestricted() {
        return None;
    }
    Some(PolicyLayer {
        bucket_key,
        limits,
        // 花费上限是经典形态的字段：它要靠 `window` 才能投影到某个计数器，
        // 而条件形态的行没有 `window`（`limits` 自带 7 个维度）。两种形态
        // 互斥，所以条件行永远不带花费层。
        spend: None,
    })
}

/// Escape a `group_by` segment value for the bucket key. CP-written
/// values are UUIDs/catalog ids and pass through untouched; the file
/// source lets operators pick arbitrary team/member id strings, where
/// an embedded `:` or `=` could otherwise alias two distinct value
/// tuples onto one bucket (`team="t:member=x"` vs `member="x"`).
fn escape_bucket_segment(value: &str) -> std::borrow::Cow<'_, str> {
    if value.contains([':', '=', '%']) {
        std::borrow::Cow::Owned(
            value
                .replace('%', "%25")
                .replace(':', "%3A")
                .replace('=', "%3D"),
        )
    } else {
        std::borrow::Cow::Borrowed(value)
    }
}

/// 就 `(policy, window, dimension)` 警告一次：某个维度在该窗口下无法执行。
/// 策略在控制平面看起来是被接受的，所以静默会让它读起来像生效了。
///
/// `classic_rate_limit` 每个请求都会走一遍，所以这里必须去重，否则每个请求
/// 都会在 warn 级别刷一行日志，把运维真正需要看到的事件淹没。Mirrors
/// `warn_partial_compat_deduped`（etcd loader），包括它的锁污染容忍：这个
/// 集合只用来给日志去重，锁下发生 panic 不能把请求路径卡死。
fn warn_inert_dimension_once(policy_name: &str, window: &str, dimension: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    // 按策略数量为界，正常情况下很小；这个上限只是防病态配置的兜底，不是工作限制。
    const MAX_REMEMBERED: usize = 1024;
    static SEEN: OnceLock<Mutex<HashSet<(String, String, String)>>> = OnceLock::new();

    let entry = (
        policy_name.to_string(),
        window.to_string(),
        dimension.to_string(),
    );
    let mut seen = SEEN
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if seen.contains(&entry) {
        return;
    }
    tracing::warn!(
        policy = %policy_name,
        window = %window,
        dimension = %dimension,
        "{dimension} ignored: per-{window} counter not implemented; \
         the policy is accepted but this ceiling is not enforced",
    );
    if seen.len() < MAX_REMEMBERED {
        seen.insert(entry);
    }
}

/// 就 `(policy, model)` 警告一次：该模型没有配价，所以这条策略的花费
/// 上限对它不生效。按模型去重而不是按策略——一条策略可能命中很多模型，
/// 只报第一个会让其余的隐身。
fn warn_unpriced_model_once(policy_name: &str, model: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    // 按 (策略, 模型) 组合数为界，正常情况下很小；这个上限只是防病态配置的兜底，不是工作限制。
    const MAX_REMEMBERED: usize = 4096;
    static SEEN: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();

    let entry = (policy_name.to_string(), model.to_string());
    let mut seen = SEEN
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if seen.contains(&entry) {
        return;
    }
    tracing::warn!(
        policy = %policy_name,
        model = %model,
        "spend ceiling not enforced: this model has no configured price, \
         so requests to it contribute nothing to the ceiling",
    );
    if seen.len() < MAX_REMEMBERED {
        seen.insert(entry);
    }
}

/// Convert a classic row's `window` + `max_*` into the 7-field
/// [`RateLimit`]. `None` when the row carries no window (formless rows
/// are rejected at load; this is the total fallback).
fn classic_rate_limit(policy: &RateLimitPolicy) -> Option<RateLimit> {
    let mut rl = RateLimit::default();
    match policy.window? {
        PolicyWindow::Second => {
            // Pre-fix (api7/#426): `rl.rpm = max * 60` — a
            // 5/second policy was upscaled to 300/minute, allowing
            // 60× bursts past the operator-declared cap inside any
            // single 1-second window.
            // Post-fix: native rps via `FixedWindowCounter::new(1)`.
            //
            // Tokens (`tps`) intentionally NOT wired, and this is a
            // decision rather than a gap waiting to be filled. A token
            // count is only known once the upstream has answered, so
            // every token window is charged after the fact; at a 1s
            // window essentially every request commits after the bucket
            // it was admitted against has rolled, so the cap would lag
            // by about its own width. A cap that reads as enforced and
            // is not is worse than an absent one, so the sub-minute case
            // is refused loudly below. See api7/ai-gateway#396.
            rl.rps = policy.max_requests;
            // Audit M1 (#399): warn loudly when an operator set
            // `max_tokens` on a sub-minute window. Without the warn,
            // the policy looks accepted at the control plane but the token cap
            // is silently inert until ai-gateway#396 lands.
            if policy.max_tokens.is_some() {
                warn_inert_dimension_once(&policy.name, "second", "max_tokens");
            }
            // 花费上限同理：second 窗口下同样无法生效，要报出来而不是静默丢弃。
            if policy.max_spend_micro_usd.is_some() {
                warn_inert_dimension_once(&policy.name, "second", "max_spend_micro_usd");
            }
        }
        PolicyWindow::Minute => {
            rl.rpm = policy.max_requests;
            rl.tpm = policy.max_tokens;
        }
        PolicyWindow::Hour => {
            // Pre-fix (api7/#426): `rl.rpd = max * 24` —
            // a 1000/hour policy was upscaled to 24000/day, allowing
            // the entire hourly cap to be burned in any single hour
            // with no enforcement (24× exploit shape, slower-window
            // counterpart of the "second" bug).
            // Post-fix: native rph via `FixedWindowCounter::new(3600)`.
            //
            rl.rph = policy.max_requests;
            // `tph` is a native counter now, same as `tpd`: a token cap on an
            // hour window is honoured rather than warned about. The
            // post-response commit skews attribution by at most one request's
            // duration, which against 3,600 seconds is the same negligible
            // skew `tpm` and `tpd` already carry.
            rl.tph = policy.max_tokens;
        }
        PolicyWindow::Day => {
            // Both counters are native (`token_dims`, `KeyState`, and
            // the Redis scripts handle `tpd` already) — day is the one
            // window where a token cap on a team-capable policy was
            // previously inexpressible (#771).
            rl.rpd = policy.max_requests;
            rl.tpd = policy.max_tokens;
        }
    }
    Some(rl)
}

/// 花费层的桶键。与 token 层的 `policy:` 前缀分开：同一策略的两个维度
/// 共用一个桶，token 数会把花费额度吃掉，且不会有任何报错。
///
/// `team_member` 作用域的成员后缀由调用方拼接，与 token 桶保持同一形状。
fn spend_bucket_key(scope: &str, scope_ref: &str, policy_entry_id: &str) -> String {
    format!("spend:{scope}:{scope_ref}:{policy_entry_id}")
}

/// 把花费上限投影到与 token 相同的窗口字段。
/// `second` 返回全空——见 [`warn_inert_dimension_once`] 的调用点。
fn spend_limits_for(window: PolicyWindow, max_spend_micro_usd: u64) -> RateLimit {
    let mut rl = RateLimit::default();
    match window {
        PolicyWindow::Second => {}
        PolicyWindow::Minute => rl.tpm = Some(max_spend_micro_usd),
        PolicyWindow::Hour => rl.tph = Some(max_spend_micro_usd),
        PolicyWindow::Day => rl.tpd = Some(max_spend_micro_usd),
    }
    rl
}

/// Reserve across all applicable rate-limit layers (api_key, model,
/// mcp_server, policies). `mcp_server` is the MCP server an in-flight
/// `tools/call` targets; `None` for every non-MCP endpoint.
async fn reserve_layers(
    state: &ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    auth: &AuthenticatedKey,
    model_rl: Option<&ModelRateLimit>,
    mcp_server: Option<&str>,
) -> Result<MultiReservation, ProxyError> {
    // Starts empty so the common no-limits request never allocates;
    // the first reservation (if any) grows it on demand.
    let mut reservations = Vec::new();

    // Layer 1: API key inline rate limit.
    let key_limits = auth.key().rate_limit.clone().unwrap_or_default();
    if !key_limits.is_unrestricted() {
        let r = state
            .limiter
            .pre_commit(&auth.entry.id, &key_limits)
            .await
            .map_err(|e| reject(state, e, "api_key", None))?;
        reservations.push(r);
    }

    // Layer 2: Model inline rate limit.
    if let Some(mrl) = model_rl {
        if let Some(ref limits) = mrl.limits {
            let key = format!("model:{}", mrl.bucket_name);
            let r = state
                .limiter
                .pre_commit(&key, limits)
                .await
                .map_err(|e| reject(state, e, "model", None))?;
            reservations.push(r);
        }
    }

    // Layer 3: per-MCP-server limit carried by this key. Bucketed on
    // `mcp:<api_key_id>:<server>` so each server the key reaches counts
    // independently — of the other servers, and of every other key.
    if let Some(server) = mcp_server {
        if let Some(limits) = auth.key().mcp_rate_limit(server) {
            let rl = RateLimit::from(limits);
            if !rl.is_unrestricted() {
                let key = format!("mcp:{}:{}", auth.entry.id, server);
                let r = state
                    .limiter
                    .pre_commit(&key, &rl)
                    .await
                    .map_err(|e| reject(state, e, "mcp", None))?;
                reservations.push(r);
            }
        }
    }

    // Layer 4+: Rate limit policies from snapshot.
    let input = condition_input(auth, model_rl, None);
    let phase = PolicyPhase::Request {
        defer_model_properties: model_rl.is_some_and(|m| m.routing_parent),
    };
    reserve_policy_layers(state, snapshot, &input, phase, &mut reservations).await?;

    Ok(MultiReservation::new(reservations))
}

/// Scan the policy table once for the given phase and reserve every
/// applicable layer. Shared by the request gate ([`reserve_layers`])
/// and the per-target gate ([`reserve_model_only`]) so the two scans
/// cannot drift (the schedules gate had to be patched into both loops
/// once already — #1104).
async fn reserve_policy_layers(
    state: &ProxyState,
    // The caller's request snapshot (#941). This gate used to load its
    // own, which on a zero-policy deployment was the entire cost of the
    // call — the emptiness check below is O(1).
    snap: &aisix_core::AisixSnapshot,
    input: &ConditionInput<'_>,
    phase: PolicyPhase,
    reservations: &mut Vec<aisix_ratelimit::Reservation>,
) -> Result<(), ProxyError> {
    // O(1) empty check before anything else: deployments with no
    // rate-limit policies (the default) skip the wall-clock read and
    // the per-shard table scan below entirely. Covers both callers —
    // the request gate and the per-target gate.
    if snap.rate_limit_policies.is_empty() {
        return Ok(());
    }
    let now = chrono::Utc::now();
    for entry in snap.rate_limit_policies.entries() {
        let policy = &entry.value;
        // Inside a scheduled suspension window the policy reserves
        // nothing; enforcement resumes automatically when the window
        // closes, on the unchanged bucket (#1104).
        if policy.suspended_at(now) {
            continue;
        }
        let Some(layer) = match_policy_layer(policy, &entry.id, input, phase) else {
            continue;
        };
        // 只设了花费上限的策略在 token 侧是无限制的，不必占一个空层。
        if !layer.limits.is_unrestricted() {
            let r = state
                .limiter
                .pre_commit(&layer.bucket_key, &layer.limits)
                .await
                .map_err(|e| reject(state, e, "policy", Some((&entry.id, &policy.name))))?;
            reservations.push(r);
        }
        if let Some(spend) = layer.spend {
            // 与 token 层同策略同窗口，独立桶与单位；任一超限即拒。
            // 花费层超限要报成 `BudgetExceeded`（billing_error），不是
            // token/请求层那种 `PolicyRateLimit`（rate_limit_exceeded）——
            // 两者同为 429，但客户端据此区分"钱用完了"和"请求太快了"，
            // 处置完全不同。只有这里（花费层）走这条路径；token 层的
            // `reject` 调用点不变。
            let r = state
                .limiter
                .pre_commit_with_unit(&spend.bucket_key, &spend.limits, CounterUnit::MicroUsd)
                .await
                .map_err(|e| reject_spend(state, e, &entry.id, policy))?;
            reservations.push(r);

            // 未定价模型对花费计数器贡献 0，所以这个上限对它无效。
            // 放行，但不静默——拒绝会把一个配置疏漏变成流量中断。
            //
            // R10：只在能给出准确结论的地方断言"未定价"。
            // 直接模型（含 embedding）：`cost` 就是它自己的定价，直接判断。
            // 虚拟父级（routing/ensemble/semantic）：此刻还不知道会调度到
            // 哪个具体目标，而这类行结构上永远不带 `cost`——真实花费是按
            // *调度到的目标* 定价的（`usage_attr::request_cost_usd` 吃的是
            // 实际调度目标的 model_id，不是这里的父级 id），可能非零。对
            // 父级断言"未定价"会把一条计费正常的请求错误地标成"预算失
            // 效"，这是假信号，比留白更糟——见
            // docs/design/2026-08-20-spend-budget-design.md 的 Non-goals：
            // 通过虚拟父级调度到的、真正未定价的目标，这里不会产生任何
            // 信号，是已知盲区。
            if let (Some(model_id), Some(model_name)) = (input.model, input.model_name) {
                if let Some(resolved) = snap.models.get_by_id(model_id) {
                    let model = &resolved.value;
                    let is_virtual_parent =
                        model.is_routing() || model.is_ensemble() || model.is_semantic();
                    if !is_virtual_parent && model.cost.is_none() {
                        // 标签必须用已解析的行名：通配符路径下调用方能自造模型名，
                        // 直接打标签会让这个 series 基数无上限。
                        let label = crate::usage_attr::metric_model_label(snap, model_name);
                        state.metrics.record_budget_unpriced(&policy.name, &label);
                        warn_unpriced_model_once(&policy.name, &label);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Convert a store-level rejection into the surfaced [`ProxyError`],
/// counting it under `aisix_ratelimit_rejections_total{scope,layer}` —
/// the gate is the one point every endpoint funnels through, so the
/// counter covers them all. Policy-layer rejections carry the policy
/// identity for 429 attribution (`error.policy`, #892).
fn reject(
    state: &ProxyState,
    err: aisix_ratelimit::RateLimitError,
    layer: &'static str,
    policy: Option<(&str, &str)>,
) -> ProxyError {
    state.metrics.record_ratelimit_rejection(
        &err.scope().to_string(),
        layer,
        policy.map(|(id, _)| id),
    );
    match policy {
        Some((id, name)) => ProxyError::PolicyRateLimit {
            source: err,
            policy_id: id.to_string(),
            policy_name: name.to_string(),
        },
        None => ProxyError::from(err),
    }
}

/// 花费层专属的拒绝路径，和 [`reject`] 分开：花费桶超限要报成
/// `ProxyError::BudgetExceeded`（`billing_error`），不是通用的
/// `ProxyError::PolicyRateLimit`（`rate_limit_exceeded`）——两者同为
/// 429，但客户端据此区分"钱用完了"和"请求太快了"，这是两种不同的处置。
///
/// 指标的 `layer` 标签同样要和 token 层分开（`policy_spend`，固定字符串，
/// 不掺调用方数据），否则花费拒绝会计入 token 限流的层位，两者在
/// `aisix_ratelimit_rejections_total` 上无法区分。
///
/// 花费层只在经典形态的策略行上构造（见 [`match_conditional_layer`] 里
/// `spend: None` 的注释），走到这里时 `policy.scope`/`policy.scope_ref`
/// 一定是 `Some`（[`match_policy_layer`] 已经检查过一次）。
fn reject_spend(
    state: &ProxyState,
    err: aisix_ratelimit::RateLimitError,
    policy_id: &str,
    policy: &RateLimitPolicy,
) -> ProxyError {
    state.metrics.record_ratelimit_rejection(
        &err.scope().to_string(),
        "policy_spend",
        Some(policy_id),
    );
    let retry_after_seconds = err.retry_after_secs();
    let usd = |micro: u64| format!("{:.6}", micro as f64 / 1_000_000.0);
    ProxyError::BudgetExceeded(Box::new(crate::budget_reason::BudgetReason {
        message: format!("spend ceiling reached for policy {}", policy.name),
        scope: policy.scope.map(|s| s.as_str().to_string()),
        scope_ref: policy.scope_ref.clone(),
        limit_usd: Some(usd(policy.max_spend_micro_usd.unwrap_or_default())),
        // 窗口计数器（`FixedWindowCounter`）只按桶存当前累加值，不区分
        // "花费"与"token"这两种量纲的读回接口——`Limiter::peek`
        // （store/local.rs:263-287）确实存在，但它只读 rpm/tpm 这两个
        // 分钟窗口计数器，从不读 tph/tpd。对一条 hour/day 窗口的花费策略
        // 调它，回读到的会是"当前这一分钟"的花费，冒充整个周期的花费
        // ——一个看起来权威、实际算错窗口的数字比留白更糟。要修 `peek`
        // 本身去认窗口，是 store 层的改动，本任务的约束不允许。
        // 上限 + 重试时间已足够客户端处置。
        spent_usd: None,
        period: policy.window.map(|w| w.as_str().to_string()),
        period_resets_at: None,
        retry_after_seconds,
    }))
}

/// Apply multi-layer rate-limit checks for one request (this includes the
/// spend/budget layer projected from a policy's `max_spend_micro_usd`, see
/// `PolicyLayer::spend` above). `model_rl` carries the resolved model
/// identity for policy matching and optional inline limits. Pass `None`
/// only for endpoints that don't resolve a model (e.g. passthrough).
///
/// 预算不再由这里单独判定：过去这里会先问一次控制平面（HTTP `check_budget`）
/// 再走限流，现在预算就是策略里的一层花费桶，和其余限流层一起在
/// `reserve_layers` 里预留/提交。花费桶超限时报 `ProxyError::
/// BudgetExceeded`（见 [`reject_spend`]），与 token/请求层的
/// `PolicyRateLimit` 区分开。
pub(crate) async fn enforce(
    state: &ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    auth: &AuthenticatedKey,
    model_rl: Option<&ModelRateLimit>,
) -> Result<MultiReservation, ProxyError> {
    reserve_layers(state, snapshot, auth, model_rl, None).await
}

/// Apply multi-layer rate-limit checks for one MCP `tools/call` against
/// `mcp_server`, the server the called tool belongs to. Same gate as
/// [`enforce`] plus the key's per-server layer; MCP resolves no model, so
/// the model layers are never engaged.
pub(crate) async fn enforce_mcp(
    state: &ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    auth: &AuthenticatedKey,
    mcp_server: &str,
) -> Result<MultiReservation, ProxyError> {
    reserve_layers(state, snapshot, auth, None, Some(mcp_server)).await
}

/// Alias for [`enforce`], kept so `chat.rs`'s call site doesn't need a
/// rename. Historically distinct because chat dispatch ran its own inline
/// control-plane budget check before calling this; that inline check is
/// gone (budgets are no longer decided by an HTTP call), so the bodies were
/// byte-identical duplicates — consolidated here into one delegation
/// instead of two copies that could drift.
pub(crate) async fn enforce_rate_limit(
    state: &ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    auth: &AuthenticatedKey,
    model_rl: Option<&ModelRateLimit>,
) -> Result<MultiReservation, ProxyError> {
    enforce(state, snapshot, auth, model_rl).await
}

/// Reserve ONLY the model-scoped layers for one model, identified by its
/// display name + entry id: the model's inline `rate_limit`, `model`-scope
/// classic `RateLimitPolicy` rows, and conditional rows referencing a model
/// property (their request-level twin reserved at the request gate).
///
/// The ensemble fan-out uses this per sub-call: each panel member and the
/// judge is a separate upstream call that must honor its own model limits,
/// even though the request-level layers (api_key / team / member) are reserved
/// once on the entry alias and committed with the aggregate (#620). It
/// deliberately omits those request-level layers so they are not double-counted
/// per member. Returns an empty [`MultiReservation`] (zero overhead, no
/// `pre_commit` calls) when the model carries no limits, so unlimited members
/// pay nothing. On a partial failure the already-acquired layers release on the
/// dropped `Vec`, same as [`reserve_layers`].
///
/// `auth` supplies the identity dimensions a conditional row's tree may
/// combine with its model condition (e.g. `team ∈ {T} AND model_name ~~
/// ^gpt-4`); classic model-scope rows keep ignoring it (their bucket
/// never splits per user).
pub(crate) async fn reserve_model_only(
    state: &ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    auth: &AuthenticatedKey,
    model_name: &str,
    model_entry_id: &str,
    model: &aisix_core::Model,
    routing_parent: Option<RoutingParent<'_>>,
) -> Result<MultiReservation, ProxyError> {
    let mut reservations = Vec::new();

    // Inline model rate limit.
    let mrl = ModelRateLimit::from_model(model_name, model_entry_id, model);
    if let Some(ref limits) = mrl.limits {
        let key = format!("model:{}", mrl.bucket_name);
        let r = state
            .limiter
            .pre_commit(&key, limits)
            .await
            .map_err(|e| reject(state, e, "model", None))?;
        reservations.push(r);
    }

    // Policies that follow the model to this target. The condition
    // input carries the {target, caller-addressed parent} pair so a
    // policy pinning the parent's id or alias matches here too
    // (#1267).
    let input = condition_input(auth, Some(&mrl), routing_parent);
    reserve_policy_layers(
        state,
        snapshot,
        &input,
        PolicyPhase::ModelTarget,
        &mut reservations,
    )
    .await?;

    Ok(MultiReservation::new(reservations))
}

/// Reserve the model-scoped layers for one routing-dispatch target (Model
/// Group / semantic-router member), mirroring the ensemble per-sub-call
/// reservation (#620). Returns `Ok(None)` for a direct (non-routing)
/// dispatch: there the target IS the requested entry, whose model layers
/// were already reserved pre-dispatch by [`enforce`]/[`enforce_rate_limit`],
/// so reserving again would double-count the request (#1087).
///
/// An `Err` means this target is over one of its own limits right now —
/// the dispatch loops treat that as a failed 429 attempt and continue with
/// the remaining targets (matching LiteLLM, which filters rate-limited
/// deployments out of the candidate set).
pub(crate) async fn reserve_routing_target(
    state: &ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    auth: &AuthenticatedKey,
    routing_parent: Option<RoutingParent<'_>>,
    target_name: &str,
    target_entry_id: &str,
    target: &aisix_core::Model,
) -> Result<Option<MultiReservation>, ProxyError> {
    let Some(parent) = routing_parent else {
        return Ok(None);
    };
    reserve_model_only(
        state,
        snapshot,
        auth,
        target_name,
        target_entry_id,
        target,
        Some(parent),
    )
    .await
    .map(Some)
}

/// Seconds until the offending window reopens, for a
/// [`reserve_routing_target`] rejection. `chat.rs` funnels its rejection
/// through a `BridgeError`, which would otherwise drop the hint the
/// `/v1/messages` and `/v1/responses` loops keep by carrying the
/// `ProxyError::RateLimit` itself — so every endpoint's all-targets-exhausted
/// 429 lands with the same `Retry-After`.
pub(crate) fn retry_after_of(err: &ProxyError) -> Option<u64> {
    err.retry_after_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_policy(window: &str, max_req: Option<u64>, max_tok: Option<u64>) -> RateLimitPolicy {
        serde_json::from_value(serde_json::json!({
            "name": "test",
            "scope": "team",
            "scope_ref": "ref",
            "window": window,
            "max_requests": max_req,
            "max_tokens": max_tok,
        }))
        .unwrap()
    }

    fn make_scoped_policy(scope: &str, scope_ref: &str) -> RateLimitPolicy {
        serde_json::from_value(serde_json::json!({
            "name": "test",
            "scope": scope,
            "scope_ref": scope_ref,
            "window": "minute",
            "max_requests": 10,
        }))
        .unwrap()
    }

    fn make_auth(team_id: Option<&str>, user_id: Option<&str>) -> AuthenticatedKey {
        let key: aisix_core::ApiKey = serde_json::from_value(serde_json::json!({
            "key_hash": "h",
            "allowed_models": [],
            "team_id": team_id,
            "user_id": user_id,
        }))
        .unwrap();
        AuthenticatedKey {
            entry: std::sync::Arc::new(aisix_core::resource::ResourceEntry::new(
                "key-entry-1",
                key,
                1,
            )),
            jwt: None,
        }
    }

    fn make_conditional_policy(body: serde_json::Value) -> RateLimitPolicy {
        serde_json::from_value(body).unwrap()
    }

    fn make_model_rl(name: &str, entry_id: &str, provider: Option<&str>) -> ModelRateLimit {
        ModelRateLimit {
            name: name.to_owned(),
            bucket_name: name.to_owned(),
            entry_id: entry_id.to_owned(),
            limits: None,
            provider: provider.map(str::to_owned),
            routing_parent: false,
        }
    }

    /// 只带花费上限（可选叠加 token 上限）的经典策略。`RateLimitPolicy` 的
    /// `runtime_id` 是 `pub(crate)`，本 crate 构造不出结构体字面量，所以和
    /// 同模块的其他 helper 一样走 serde。
    fn make_spend_policy(
        scope: &str,
        scope_ref: &str,
        window: &str,
        max_tokens: Option<u64>,
        max_spend_micro_usd: Option<u64>,
    ) -> RateLimitPolicy {
        serde_json::from_value(serde_json::json!({
            "name": "both",
            "scope": scope,
            "scope_ref": scope_ref,
            "window": window,
            "max_tokens": max_tokens,
            "max_spend_micro_usd": max_spend_micro_usd,
        }))
        .unwrap()
    }

    /// 装了一条策略的快照。
    fn snapshot_with_policy(policy: RateLimitPolicy) -> aisix_core::AisixSnapshot {
        let snap = aisix_core::AisixSnapshot::new();
        snap.rate_limit_policies
            .insert(aisix_core::resource::ResourceEntry::new("pol-1", policy, 1));
        snap
    }

    /// 带上述快照的 ProxyState。测试构建下 `ProxyState::new` 会装一个冻结时钟，
    /// 所以同一个测试里的多次请求必定落在同一个窗口内。
    fn test_state(snap: &aisix_core::AisixSnapshot) -> crate::state::ProxyState {
        crate::state::ProxyState::new(
            aisix_core::snapshot::SnapshotHandle::new(snap.clone()),
            std::sync::Arc::new(aisix_gateway::Hub::new()),
            &aisix_core::ProxyConfig {
                addr: "127.0.0.1:0".into(),
                request_body_limit_bytes: None,
                tls: None,
                real_ip: Default::default(),
                request_id: Default::default(),
                thread_per_core: None,
                workers: None,
                url_rewrites: Vec::new(),
            },
        )
    }

    /// 指定资源 id 的 api key。策略 `scope: api_key` 时 `scope_ref` 要对上它。
    fn test_auth_key(entry_id: &str) -> AuthenticatedKey {
        let key: aisix_core::ApiKey = serde_json::from_value(serde_json::json!({
            "key_hash": "h",
            "allowed_models": [],
        }))
        .unwrap();
        AuthenticatedKey {
            entry: std::sync::Arc::new(aisix_core::resource::ResourceEntry::new(entry_id, key, 1)),
            jwt: None,
        }
    }

    const REQUEST: PolicyPhase = PolicyPhase::Request {
        defer_model_properties: false,
    };
    const REQUEST_DEFERRING: PolicyPhase = PolicyPhase::Request {
        defer_model_properties: true,
    };

    /// Classic-row bucket key via the unified matcher, at the request
    /// phase with no model resolved.
    fn classic_layer_key(
        policy: &RateLimitPolicy,
        entry_id: &str,
        auth: &AuthenticatedKey,
    ) -> String {
        match_policy_layer(
            policy,
            entry_id,
            &condition_input(auth, None, None),
            REQUEST,
        )
        .expect("policy applies")
        .bucket_key
    }

    #[test]
    fn team_member_bucket_key_is_per_user() {
        let policy = make_scoped_policy("team_member", "team-1");
        let auth_a = make_auth(Some("team-1"), Some("user-a"));
        let auth_b = make_auth(Some("team-1"), Some("user-b"));

        let key_a = classic_layer_key(&policy, "pol-1", &auth_a);
        let key_b = classic_layer_key(&policy, "pol-1", &auth_b);

        // Same team + same policy, but distinct members → distinct buckets,
        // so member A exhausting the default never throttles member B.
        assert_eq!(key_a, "policy:team_member:team-1:pol-1:user-a");
        assert_eq!(key_b, "policy:team_member:team-1:pol-1:user-b");
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn team_bucket_key_is_shared_across_members() {
        // Contrast with `team`: one bucket for the whole team regardless
        // of which member sends the request (pooled quota).
        let policy = make_scoped_policy("team", "team-1");
        let key_a = classic_layer_key(&policy, "pol-1", &make_auth(Some("team-1"), Some("user-a")));
        let key_b = classic_layer_key(&policy, "pol-1", &make_auth(Some("team-1"), Some("user-b")));
        assert_eq!(key_a, "policy:team:team-1:pol-1");
        assert_eq!(key_a, key_b);
    }

    #[test]
    fn minute_maps_to_rpm_tpm() {
        let rl = classic_rate_limit(&make_policy("minute", Some(100), Some(50000))).unwrap();
        assert_eq!(rl.rpm, Some(100));
        assert_eq!(rl.tpm, Some(50000));
        assert!(rl.rpd.is_none());
        assert!(rl.tpd.is_none());
    }

    // Regression guard for api7/#426. Pre-fix these tests
    // asserted the BUG: `second` → `rpm = max * 60` and `hour` →
    // `rpd = max * 24`. The upscaling allowed 60× and 24× bursts past
    // the operator-declared cap. Post-fix asserts the new contract:
    // `second` produces a native rps and `hour` produces a native rph.
    #[test]
    fn second_maps_to_rps_not_rpm_times_sixty() {
        let rl = classic_rate_limit(&make_policy("second", Some(10), Some(1000))).unwrap();
        assert_eq!(
            rl.rps,
            Some(10),
            "second window must populate rps natively, not rpm*60"
        );
        // No upscale into rpm/tpm — that was the #426 bug.
        assert!(
            rl.rpm.is_none(),
            "second window MUST NOT populate rpm (would 60× the cap)"
        );
        assert!(
            rl.tpm.is_none(),
            "second window MUST NOT populate tpm (would 60× the cap)"
        );
        // tps is a deliberate non-feature — see the branch comment and
        // `second_window_still_carries_no_token_cap`.
    }

    #[test]
    fn hour_maps_to_rph_not_rpd_times_twentyfour() {
        let rl = classic_rate_limit(&make_policy("hour", Some(1000), Some(500000))).unwrap();
        assert_eq!(
            rl.rph,
            Some(1000),
            "hour window must populate rph natively, not rpd*24"
        );
        // No upscale into rpd/tpd — that was the parallel #426 bug.
        assert!(
            rl.rpd.is_none(),
            "hour window MUST NOT populate rpd (would 24× the cap)"
        );
        assert!(
            rl.tpd.is_none(),
            "hour window MUST NOT populate tpd (would 24× the cap)"
        );
        assert_eq!(
            rl.tph,
            Some(500_000),
            "an hour window's token cap is a native tph counter (#396)"
        );
    }

    /// A sub-minute window still refuses to carry a token cap, and the
    /// warn-once path is what tells the operator. Pinned so wiring `tph`
    /// cannot be mistaken for having wired `tps` too.
    #[test]
    fn second_window_still_carries_no_token_cap() {
        let rl = classic_rate_limit(&make_policy("second", Some(10), Some(1000))).unwrap();
        assert!(
            rl.tph.is_none() && rl.tpm.is_none() && rl.tpd.is_none(),
            "a per-second policy's max_tokens must not leak into a wider window, \
             which would enforce a cap the operator never asked for",
        );
    }

    #[test]
    fn day_maps_to_rpd_tpd() {
        // #771: day is the one window whose token counter (`tpd`) is
        // already native end to end, so both limits wire through.
        let rl = classic_rate_limit(&make_policy("day", Some(10000), Some(2000000))).unwrap();
        assert_eq!(rl.rpd, Some(10000));
        assert_eq!(rl.tpd, Some(2000000));
        assert!(rl.rpm.is_none());
        assert!(rl.tpm.is_none());
        assert!(rl.rps.is_none());
        assert!(rl.rph.is_none());
    }

    #[test]
    fn minute_window_unchanged_by_426() {
        // Regression guard: the minute branch was always correct
        // (rpm/tpm map 1:1). #426 must not have touched it.
        let rl = classic_rate_limit(&make_policy("minute", Some(60), Some(30000))).unwrap();
        assert_eq!(rl.rpm, Some(60));
        assert_eq!(rl.tpm, Some(30000));
        assert!(rl.rps.is_none());
        assert!(rl.rph.is_none());
        assert!(rl.rpd.is_none());
    }

    #[test]
    fn unknown_window_is_rejected_at_deserialize() {
        // `PolicyWindow` is a closed enum, so an unknown window is rejected at
        // deserialize rather than silently producing an unrestricted limit.
        let r: Result<RateLimitPolicy, _> = serde_json::from_value(serde_json::json!({
            "name": "test",
            "scope": "team",
            "scope_ref": "ref",
            "window": "week",
            "max_requests": 100,
        }));
        assert!(r.is_err());
    }

    #[test]
    fn partial_fields_only_set_relevant_dimension() {
        let rl = classic_rate_limit(&make_policy("minute", Some(60), None)).unwrap();
        assert_eq!(rl.rpm, Some(60));
        assert!(rl.tpm.is_none());
    }

    // ---- spend ceiling projection (#spend-budget) ----

    /// 花费桶与 token 桶必须分开：同一策略的两个维度共用一个桶，
    /// token 数会把花费额度吃掉（或反过来），两种都静默。
    #[test]
    fn spend_bucket_is_namespaced_apart_from_the_token_bucket() {
        let tok = format!("policy:{}:{}:{}", "api_key", "k1", "p1");
        let spend = spend_bucket_key("api_key", "k1", "p1");
        assert_ne!(tok, spend);
        assert!(spend.starts_with("spend:"), "得到 {spend}");
    }

    /// 花费投影到与 token 相同的窗口字段。
    #[test]
    fn spend_projects_onto_the_matching_window_field() {
        for (window, pick) in [
            (
                PolicyWindow::Minute,
                (|r: &RateLimit| r.tpm) as fn(&RateLimit) -> Option<u64>,
            ),
            (
                PolicyWindow::Hour,
                (|r: &RateLimit| r.tph) as fn(&RateLimit) -> Option<u64>,
            ),
            (
                PolicyWindow::Day,
                (|r: &RateLimit| r.tpd) as fn(&RateLimit) -> Option<u64>,
            ),
        ] {
            let rl = spend_limits_for(window, 5_000_000);
            assert_eq!(pick(&rl), Some(5_000_000), "{window:?} 未投影");
        }
    }

    /// second 窗口下花费不生效——和 max_tokens 同样的理由，同样要报出来。
    #[test]
    fn spend_on_a_second_window_is_reported_not_silently_dropped() {
        let rl = spend_limits_for(PolicyWindow::Second, 5_000_000);
        assert_eq!(rl.tpm, None);
        assert_eq!(rl.tph, None);
        assert_eq!(rl.tpd, None);
    }

    // ---- conditional form (#892) ----

    #[test]
    fn conditional_shared_bucket_and_limits() {
        let policy = make_conditional_policy(serde_json::json!({
            "name": "team-pool",
            "conditions": [
                { "dimension": "team", "operator": "in", "value": ["team-1"] }
            ],
            "limits": { "rpm": 100 },
        }));
        let auth = make_auth(Some("team-1"), Some("user-a"));
        let layer = match_policy_layer(
            &policy,
            "pol-1",
            &condition_input(&auth, None, None),
            REQUEST,
        )
        .expect("matches");
        // No group_by → one shared bucket for every matched request.
        assert_eq!(layer.bucket_key, "policy:v2:pol-1");
        assert_eq!(layer.limits.rpm, Some(100));
    }

    #[test]
    fn group_by_segments_follow_canonical_order() {
        // Declared [model, team]; the bucket key must order team before
        // model so declaration order never changes the bucket identity.
        let policy = make_conditional_policy(serde_json::json!({
            "name": "per-team-per-model",
            "conditions": [],
            "group_by": ["model", "team"],
            "limits": { "rpm": 5 },
        }));
        let auth = make_auth(Some("team-1"), None);
        let mrl = make_model_rl("gpt-4.1-prod", "model-1", Some("openai"));
        let layer = match_policy_layer(
            &policy,
            "pol-2",
            &condition_input(&auth, Some(&mrl), None),
            REQUEST,
        )
        .expect("matches");
        assert_eq!(
            layer.bucket_key,
            "policy:v2:pol-2:team=team-1:model=model-1"
        );
    }

    #[test]
    fn group_by_missing_dimension_skips_policy() {
        // Mirrors team_member semantics: a per-member split cannot apply
        // to a key that carries no user_id.
        let policy = make_conditional_policy(serde_json::json!({
            "name": "per-member",
            "conditions": [
                { "dimension": "team", "operator": "==", "value": "team-1" }
            ],
            "group_by": ["member"],
            "limits": { "rpm": 20 },
        }));
        let auth = make_auth(Some("team-1"), None);
        assert!(match_policy_layer(
            &policy,
            "pol-3",
            &condition_input(&auth, None, None),
            REQUEST
        )
        .is_none());
    }

    #[test]
    fn model_property_policy_defers_to_target_phase_on_routing() {
        let policy = make_conditional_policy(serde_json::json!({
            "name": "gpt4-family",
            "conditions": [
                { "dimension": "model_name", "operator": "~~", "value": "^gpt-4" }
            ],
            "limits": { "rpm": 10 },
        }));
        let auth = make_auth(Some("team-1"), None);
        let parent = make_model_rl("gpt4-group", "group-1", None);
        let input = condition_input(&auth, Some(&parent), None);
        // Request gate of a routing dispatch: deferred even though the
        // parent's name would match — the concrete target decides.
        assert!(match_policy_layer(&policy, "pol-4", &input, REQUEST_DEFERRING).is_none());
        // Per-target gate: matches the concrete target.
        let target = make_model_rl("gpt-4.1-prod", "model-1", Some("openai"));
        let target_input = condition_input(&auth, Some(&target), None);
        let layer = match_policy_layer(&policy, "pol-4", &target_input, PolicyPhase::ModelTarget)
            .expect("target matches");
        assert_eq!(layer.bucket_key, "policy:v2:pol-4");
    }

    #[test]
    fn group_referencing_policy_matches_at_target_phase_via_parent() {
        // #1267: `model in [group uuid]` reserves at the
        // per-target gate because the condition input carries the
        // {target, parent} pair — previously the parent id was compared
        // nowhere and the policy never fired.
        let policy = make_conditional_policy(serde_json::json!({
            "name": "group-cap",
            "conditions": [
                { "dimension": "model", "operator": "in", "value": ["group-1"] }
            ],
            "group_by": ["member"],
            "limits": { "rph": 3 },
        }));
        let auth = make_auth(Some("team-1"), Some("user-a"));
        // Request gate of the routing dispatch: still deferred.
        let gate = make_model_rl("chat-group", "group-1", None);
        let gate_input = condition_input(&auth, Some(&gate), None);
        assert!(match_policy_layer(&policy, "pol-9", &gate_input, REQUEST_DEFERRING).is_none());
        // Per-target gate: the parent pair makes it match, bucketed per
        // member.
        let target = make_model_rl("gpt-4.1-prod", "model-1", Some("openai"));
        let parent = RoutingParent {
            name: "chat-group",
            entry_id: "group-1",
        };
        let input = condition_input(&auth, Some(&target), Some(parent));
        let layer = match_policy_layer(&policy, "pol-9", &input, PolicyPhase::ModelTarget)
            .expect("group-referencing policy matches via the parent");
        assert_eq!(layer.bucket_key, "policy:v2:pol-9:member=user-a");
        // Direct dispatch to the member (no parent): must NOT match.
        let direct = condition_input(&auth, Some(&target), None);
        assert!(match_policy_layer(&policy, "pol-9", &direct, REQUEST).is_none());
    }

    #[test]
    fn group_by_model_buckets_on_target_not_parent() {
        // The pair extends MATCHING only: a group-referencing policy
        // splitting by model still buckets on the dispatched target id,
        // so per-member counters stay per concrete model.
        let policy = make_conditional_policy(serde_json::json!({
            "name": "group-per-model",
            "conditions": [
                { "dimension": "model", "operator": "in", "value": ["group-1"] }
            ],
            "group_by": ["model"],
            "limits": { "rpm": 1 },
        }));
        let auth = make_auth(Some("team-1"), None);
        let target = make_model_rl("gpt-4.1-prod", "model-1", Some("openai"));
        let parent = RoutingParent {
            name: "chat-group",
            entry_id: "group-1",
        };
        let input = condition_input(&auth, Some(&target), Some(parent));
        let layer = match_policy_layer(&policy, "pol-10", &input, PolicyPhase::ModelTarget)
            .expect("matches via parent");
        assert_eq!(layer.bucket_key, "policy:v2:pol-10:model=model-1");
    }

    #[test]
    fn non_model_policy_not_rereserved_at_target_phase() {
        let policy = make_conditional_policy(serde_json::json!({
            "name": "team-pool",
            "conditions": [
                { "dimension": "team", "operator": "in", "value": ["team-1"] }
            ],
            "limits": { "rpm": 100 },
        }));
        let auth = make_auth(Some("team-1"), None);
        let target = make_model_rl("gpt-4.1-prod", "model-1", Some("openai"));
        let input = condition_input(&auth, Some(&target), None);
        // Reserved once at the request gate; the per-target scan must
        // not double-count it.
        assert!(match_policy_layer(&policy, "pol-5", &input, PolicyPhase::ModelTarget).is_none());
    }

    #[test]
    fn or_branch_matches_model_less_request() {
        // §3.3 rule 3: a missing dimension only fails its own leaf. An
        // MCP/A2A request (no model) still matches through the team
        // branch of an OR group — and, carrying a model-property leaf,
        // the policy is evaluated at the request gate because a
        // model-less request has no target phase.
        let policy = make_conditional_policy(serde_json::json!({
            "name": "team-or-provider",
            "conditions": [
                { "logic": "or", "children": [
                    { "dimension": "team", "operator": "==", "value": "team-1" },
                    { "dimension": "provider", "operator": "==", "value": "anthropic" }
                ]}
            ],
            "limits": { "rpm": 50 },
        }));
        let auth = make_auth(Some("team-1"), None);
        let layer = match_policy_layer(
            &policy,
            "pol-6",
            &condition_input(&auth, None, None),
            REQUEST,
        )
        .expect("matches via team branch");
        assert_eq!(layer.bucket_key, "policy:v2:pol-6");
    }

    #[test]
    fn classic_scope_rows_ignored_at_target_phase_except_model() {
        let team_policy = make_scoped_policy("team", "team-1");
        let auth = make_auth(Some("team-1"), Some("user-a"));
        let target = make_model_rl("gpt-4.1-prod", "model-1", Some("openai"));
        let input = condition_input(&auth, Some(&target), None);
        assert!(
            match_policy_layer(&team_policy, "pol-7", &input, PolicyPhase::ModelTarget).is_none()
        );

        let model_policy = make_scoped_policy("model", "model-1");
        let layer = match_policy_layer(&model_policy, "pol-8", &input, PolicyPhase::ModelTarget)
            .expect("model scope follows the target");
        assert_eq!(layer.bucket_key, "policy:model:model-1:pol-8");
    }

    /// 一个同时设了 max_tokens 与 max_spend 的策略要预留两层，
    /// 且花费层带 MicroUsd 单位。只留一层会让其中一个上限静默失效。
    #[tokio::test]
    async fn a_policy_with_both_ceilings_reserves_a_token_layer_and_a_spend_layer() {
        let snap = snapshot_with_policy(make_spend_policy(
            "api_key",
            "k1",
            "day",
            Some(1_000),
            Some(5_000_000),
        ));
        let state = test_state(&snap);
        let auth = test_auth_key("k1");

        let res = reserve_layers(&state, &snap, &auth, None, None)
            .await
            .expect("两层都应预留成功");

        assert_eq!(
            res.token_keys(),
            vec!["policy:api_key:k1:pol-1".to_string()],
            "缺 token 层"
        );
        assert_eq!(
            res.spend_keys(),
            vec!["spend:api_key:k1:pol-1".to_string()],
            "缺花费层"
        );
    }

    /// 只设花费上限的策略——预算的典型形态——也必须预留花费层。
    /// token 侧无限制不等于整条策略无限制；早退会让预算完全不执行。
    #[tokio::test]
    async fn a_spend_only_policy_still_reserves_its_spend_layer() {
        let snap = snapshot_with_policy(make_spend_policy(
            "api_key",
            "k1",
            "day",
            None,
            Some(5_000_000),
        ));
        let state = test_state(&snap);
        let auth = test_auth_key("k1");

        let res = reserve_layers(&state, &snap, &auth, None, None)
            .await
            .expect("花费层应预留成功");

        assert!(res.token_keys().is_empty(), "不该占一个空的 token 层");
        assert_eq!(res.spend_keys(), vec!["spend:api_key:k1:pol-1".to_string()]);
    }

    /// 花费层真的会被消耗：提交一次超过上限的花费之后，同一窗口的下一次
    /// 请求必须被拒。只预留不消耗的话桶永远是空的，预算看起来配了却从不
    /// 触发——和「没超预算」在外部完全无法区分。
    #[tokio::test]
    async fn a_committed_spend_consumes_the_spend_bucket() {
        let snap = snapshot_with_policy(make_spend_policy(
            "api_key",
            "k1",
            "day",
            None, // token 侧不设限，只验花费维度
            Some(1_000),
        ));
        let state = test_state(&snap);
        let auth = test_auth_key("k1");

        // 第一次放行，并按 micro-USD 提交一笔超过上限的花费。
        reserve_layers(&state, &snap, &auth, None, None)
            .await
            .expect("首次应放行")
            .commit(0, 5_000)
            .await;

        let err = reserve_layers(&state, &snap, &auth, None, None)
            .await
            .expect_err("花费已超上限，应被拒");
        assert!(
            matches!(err, ProxyError::BudgetExceeded(_)),
            "应为 BudgetExceeded，实际 {err:?}"
        );
    }

    /// 对照组：token 层（同一策略资源，只是没设花费上限）超限时必须
    /// 仍然是 `PolicyRateLimit`，不能被这次改动误伤——只有花费层的
    /// 预留失败才改口成 `BudgetExceeded`。
    #[tokio::test]
    async fn a_token_layer_breach_still_reports_policy_rate_limit_not_budget() {
        let snap = snapshot_with_policy(make_spend_policy(
            "api_key",
            "k1",
            "day",
            Some(1), // token 上限，第二次必超
            None,    // 不设花费上限——这条策略的花费层不存在
        ));
        let state = test_state(&snap);
        let auth = test_auth_key("k1");

        reserve_layers(&state, &snap, &auth, None, None)
            .await
            .expect("首次应放行")
            .commit(10_000, 0)
            .await;

        let err = reserve_layers(&state, &snap, &auth, None, None)
            .await
            .expect_err("token 已超上限，应被拒");
        assert!(
            matches!(err, ProxyError::PolicyRateLimit { .. }),
            "token/请求层超限应仍为 PolicyRateLimit，实际 {err:?}"
        );
        assert_eq!(err.status().as_u16(), 429);
        assert_eq!(err.kind(), "rate_limit_exceeded");
    }

    /// 花费超限要报成预算错误，不是通用限流错误——两者状态码相同（429），
    /// 但错误分类与 `error.budget.*` 字段不同，客户端据此区分"钱用完了"
    /// 和"请求太快了"，这是两种完全不同的处置。
    #[tokio::test]
    async fn a_spend_ceiling_breach_reports_as_budget_not_rate_limit() {
        let snap = snapshot_with_policy(make_spend_policy(
            "api_key",
            "k1",
            "day",
            None,    // token 侧不设限，只验花费维度
            Some(1), // 1 micro-USD，第二次必超
        ));
        let state = test_state(&snap);
        let auth = test_auth_key("k1");

        reserve_layers(&state, &snap, &auth, None, None)
            .await
            .expect("首次应放行")
            .commit(0, 10_000)
            .await;

        let err = reserve_layers(&state, &snap, &auth, None, None)
            .await
            .expect_err("超限应被拒");
        assert!(
            matches!(err, ProxyError::BudgetExceeded(_)),
            "应为 BudgetExceeded，实际 {err:?}"
        );
        assert_eq!(err.status().as_u16(), 429);
        assert_eq!(err.kind(), "billing_error");

        // 指标的 layer 标签也要能把花费拒绝和 token/请求层拒绝分开，
        // 否则 `aisix_ratelimit_rejections_total` 上两种拒绝会混成一个
        // series，运维没法单独观察预算命中率。标签是固定字符串
        // （"policy_spend"），不掺调用方数据。
        let rendered = state.metrics.render();
        assert!(
            rendered.contains("layer=\"policy_spend\""),
            "花费层拒绝应打独立的 layer 标签: {rendered}"
        );
    }

    /// token 数不能记进花费桶：两种桶的键表必须分开取。混在一起时数字看着
    /// 正常、量纲完全不对，且没有任何报错。
    #[tokio::test]
    async fn token_and_spend_bucket_keys_never_share_one_list() {
        let snap = snapshot_with_policy(make_spend_policy(
            "api_key",
            "k1",
            "day",
            Some(1_000),
            Some(5_000_000),
        ));
        let state = test_state(&snap);
        let auth = test_auth_key("k1");

        let res = reserve_layers(&state, &snap, &auth, None, None)
            .await
            .expect("两层都应预留成功");

        let tokens = res.token_keys();
        let spend = res.spend_keys();
        assert!(!tokens.is_empty() && !spend.is_empty());
        assert!(
            tokens.iter().all(|k| !spend.contains(k)),
            "两张键表重叠: {tokens:?} / {spend:?}"
        );
    }

    /// `team_member` 的花费桶要和 token 桶一样按成员拆分。不拆的话
    /// token 上限是每人一份、预算却是全队共用一份，且没有任何报错。
    #[test]
    fn team_member_spend_bucket_is_per_user_like_the_token_bucket() {
        let policy = make_spend_policy("team_member", "team-1", "day", Some(1_000), Some(1));
        let keys = |user: &str| {
            let auth = make_auth(Some("team-1"), Some(user));
            let layer = match_policy_layer(
                &policy,
                "pol-1",
                &condition_input(&auth, None, None),
                REQUEST,
            )
            .expect("policy applies");
            let spend = layer.spend.expect("花费层缺失");
            (layer.bucket_key, spend.bucket_key)
        };
        let (tok_a, spend_a) = keys("user-a");
        let (tok_b, spend_b) = keys("user-b");
        assert_eq!(tok_a, "policy:team_member:team-1:pol-1:user-a");
        assert_eq!(spend_a, "spend:team_member:team-1:pol-1:user-a");
        assert_ne!(tok_a, tok_b);
        assert_ne!(spend_a, spend_b);
    }

    // ---- unpriced-model visibility (Task 5) ----

    /// 未定价模型：策略配了花费上限，调度到没有 `cost` 的模型，请求照常
    /// 放行（拒绝会把配置疏漏变成流量中断），但必须留下指标痕迹——不留痕
    /// 的话，"预算配了但从不触发"和"预算没被超过"完全无法区分。
    #[tokio::test]
    async fn unpriced_model_under_a_spend_policy_is_counted() {
        let snap = snapshot_with_policy(make_spend_policy(
            "api_key",
            "k1",
            "day",
            None,
            Some(5_000_000),
        ));
        let unpriced: aisix_core::Model = serde_json::from_value(serde_json::json!({
            "display_name": "gpt-4o-mini",
            "provider": "openai",
            "model_name": "gpt-4o-mini",
            "provider_key_id": "pk-1",
        }))
        .unwrap();
        snap.models.insert(aisix_core::resource::ResourceEntry::new(
            "model-1", unpriced, 1,
        ));
        let state = test_state(&snap);
        let auth = test_auth_key("k1");
        let mrl = make_model_rl("gpt-4o-mini", "model-1", Some("openai"));

        reserve_layers(&state, &snap, &auth, Some(&mrl), None)
            .await
            .expect("未定价模型不该被拒绝，只是不对这条花费上限做贡献");

        let rendered = state.metrics.render();
        assert!(
            rendered.contains(aisix_obs::metrics::M_BUDGET_UNPRICED_REQUESTS_TOTAL),
            "未定价指标缺失: {rendered}"
        );
        assert!(
            rendered.contains("policy=\"both\""),
            "policy 标签缺失: {rendered}"
        );
        assert!(
            rendered.contains("model=\"gpt-4o-mini\""),
            "model 标签缺失: {rendered}"
        );
    }

    /// 对照组：定价模型不应产生未定价告警指标。
    #[tokio::test]
    async fn priced_model_under_a_spend_policy_is_not_counted() {
        let snap = snapshot_with_policy(make_spend_policy(
            "api_key",
            "k1",
            "day",
            None,
            Some(5_000_000),
        ));
        let priced: aisix_core::Model = serde_json::from_value(serde_json::json!({
            "display_name": "gpt-4o",
            "provider": "openai",
            "model_name": "gpt-4o",
            "provider_key_id": "pk-1",
            "cost": { "input_per_1k": 2.5, "output_per_1k": 10.0 },
        }))
        .unwrap();
        snap.models.insert(aisix_core::resource::ResourceEntry::new(
            "model-1", priced, 1,
        ));
        let state = test_state(&snap);
        let auth = test_auth_key("k1");
        let mrl = make_model_rl("gpt-4o", "model-1", Some("openai"));

        reserve_layers(&state, &snap, &auth, Some(&mrl), None)
            .await
            .expect("应放行");

        let rendered = state.metrics.render();
        assert!(
            !rendered.contains(aisix_obs::metrics::M_BUDGET_UNPRICED_REQUESTS_TOTAL),
            "定价模型不该产生未定价告警: {rendered}"
        );
    }

    /// R10：虚拟父级（routing/ensemble/semantic）在预留这一刻还不知道会
    /// 调度到哪个具体目标，而这类行结构上永远不带 `cost`。真实花费是按
    /// *调度到的目标* 定价的（可能非零），对父级断言"未定价"是假信号，
    /// 比留白更糟——这里必须什么都不产生。
    #[tokio::test]
    async fn virtual_parent_dispatch_emits_no_unpriced_signal() {
        let snap = snapshot_with_policy(make_spend_policy(
            "api_key",
            "k1",
            "day",
            None,
            Some(5_000_000),
        ));
        let parent: aisix_core::Model = serde_json::from_value(serde_json::json!({
            "display_name": "chat-group",
            "routing": { "targets": [{ "model": "gpt-4o" }] },
        }))
        .unwrap();
        snap.models.insert(aisix_core::resource::ResourceEntry::new(
            "group-1", parent, 1,
        ));
        let state = test_state(&snap);
        let auth = test_auth_key("k1");
        let mrl = make_model_rl("chat-group", "group-1", None);

        reserve_layers(&state, &snap, &auth, Some(&mrl), None)
            .await
            .expect("应放行");

        let rendered = state.metrics.render();
        assert!(
            !rendered.contains(aisix_obs::metrics::M_BUDGET_UNPRICED_REQUESTS_TOTAL),
            "虚拟父级不该产生未定价告警——此刻不知道会调度到哪个具体目标: {rendered}"
        );
    }
}
