# 花费预算本地化 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把每 api_key 的花费预算从请求路径上的同步控制平面 HTTP 调用，改为 `RateLimitPolicy` 上的一个新维度、由现有限流器本地执行。

**Architecture:** 花费是"换了单位的 token"。`RateLimitPolicy` 新增 `max_spend_micro_usd`，投影到与 `max_tokens` 相同的窗口字段，但走独立的桶命名空间 `spend:*`。`FixedWindowCounter` 与 `RateStore`（`LocalStore`/`RedisStore`）**一行不改** —— 它们不关心自己数的是什么；只有 `Reservation` 携带一个 `CounterUnit` 标签，`MultiReservation::commit` 据此把 token 数发给 token 层、把 micro-USD 发给花费层。

**Tech Stack:** Rust（axum / tokio / async-trait / schemars / serde），Vitest + 真实 etcd/Redis 的 e2e。

**Spec:** `docs/design/2026-08-20-spend-budget-design.md`

## Global Constraints

- 单位一律 **micro-USD 的 `u64`**（1 USD = 1_000_000）。计数器上不得出现 `f64`。
- `crates/aisix-ratelimit/src/store/` 下的文件**不得修改** —— store 对单位无感是本设计的核心边界。
- `RateLimit` 结构（7 字段）**不得新增字段**。
- 对调用方的预算错误 JSON 形状**不得变化**：429 + `billing_error` + `error.budget.*`。
- 注释用中文（仓库 CLAUDE.md 要求），面向公开 API 的 doc comment 写成用户可读的参考文本。
- 每个任务结束前跑 `cargo fmt --all && cargo clippy --workspace --all-targets`，必须零 warning。
- 改了 `crates/aisix-core/src/models/` 下的模型后，必须跑 `cargo run -p aisix-core --bin dump-schema` 重新生成 schema 并一并提交。

---

### Task 1: `CounterUnit` 与按单位分派的提交

**Files:**
- Modify: `crates/aisix-ratelimit/src/limiter.rs`（`Reservation` 结构在 174 行，构造在 116 行，`pre_commit` 在 97 行）
- Modify: `crates/aisix-ratelimit/src/lib.rs`（导出 `CounterUnit`）
- Test: `crates/aisix-ratelimit/src/limiter.rs` 的 `mod tests`

**Interfaces:**
- Produces: `pub enum CounterUnit { Tokens, MicroUsd }`；
  `Limiter::pre_commit_with_unit(&self, key: &str, limits: &RateLimit, unit: CounterUnit) -> Result<Reservation, RateLimitError>`；
  `MultiReservation::commit(self, tokens: u64, spend_micro_usd: u64)`。
  `pre_commit` 与 `commit_tokens` 保留原签名（44 处调用点依赖它们）。

- [ ] **Step 1: 写失败的测试**

在 `crates/aisix-ratelimit/src/limiter.rs` 的 `mod tests` 末尾加：

```rust
/// 两个层各自计不同的量：token 层收 token 数，花费层收 micro-USD。
/// 把两者搞混不会让任何请求失败——只会让预算按 token 数扣钱，
/// 或让 token 窗口按分币计数，两种都静默。
#[tokio::test]
async fn commit_dispatches_each_layer_by_its_unit() {
    let store = Arc::new(LocalStore::new());
    let limiter = Limiter::with_store(Arc::clone(&store) as Arc<dyn RateStore>);
    let limits = RateLimit {
        tpd: Some(1_000_000),
        ..RateLimit::default()
    };

    let tok = limiter
        .pre_commit_with_unit("tok-layer", &limits, CounterUnit::Tokens)
        .await
        .expect("token layer reserves");
    let spend = limiter
        .pre_commit_with_unit("spend-layer", &limits, CounterUnit::MicroUsd)
        .await
        .expect("spend layer reserves");

    MultiReservation::new(vec![tok, spend]).commit(150, 4_200).await;

    assert_eq!(store.committed_tokens("tok-layer"), 150, "token 层应收到 token 数");
    assert_eq!(store.committed_tokens("spend-layer"), 4_200, "花费层应收到 micro-USD");
}

/// 旧入口保持原语义：全部按 token 层处理，花费为 0。
/// 44 处既有调用点依赖这一点。
#[tokio::test]
async fn commit_tokens_still_treats_every_layer_as_tokens() {
    let store = Arc::new(LocalStore::new());
    let limiter = Limiter::with_store(Arc::clone(&store) as Arc<dyn RateStore>);
    let limits = RateLimit { tpd: Some(1_000), ..RateLimit::default() };
    let r = limiter.pre_commit("legacy", &limits).await.expect("reserves");
    MultiReservation::new(vec![r]).commit_tokens(77).await;
    assert_eq!(store.committed_tokens("legacy"), 77);
}
```

若 `LocalStore` 没有 `committed_tokens` 测试辅助，在 `crates/aisix-ratelimit/src/store/local.rs` 的 `mod tests` 之外加一个 `#[cfg(test)]` 方法读回 `tpd` 计数（这不算修改 store 的生产逻辑）：

```rust
#[cfg(test)]
impl LocalStore {
    /// 读回某个桶已提交的 tpd 计数，仅供测试断言。
    pub fn committed_tokens(&self, key: &str) -> u64 {
        self.keys.get(key).map(|s| s.lock().tpd.count()).unwrap_or(0)
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p aisix-ratelimit --lib commit_dispatches -- --nocapture`
Expected: FAIL，编译错误 `cannot find type CounterUnit` / `no method named pre_commit_with_unit`

- [ ] **Step 3: 实现最小改动**

在 `crates/aisix-ratelimit/src/limiter.rs` 加：

```rust
/// 一个预留层计数的单位。
///
/// store 对此无感——`FixedWindowCounter` 不关心它数的是什么，token 和钱
/// 对它是同一种量。只有 `Reservation` 知道自己那个桶里的数字是什么单位，
/// 所以分派发生在这一层，而不是store 层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterUnit {
    /// 桶里计的是 token 数。
    Tokens,
    /// 桶里计的是 micro-USD（1 USD = 1_000_000）。
    MicroUsd,
}
```

`Reservation` 加字段（174 行）：

```rust
pub struct Reservation {
    store: Arc<dyn RateStore>,
    key: String,
    member: String,
    renewal: Option<tokio::task::JoinHandle<()>>,
    committed: bool,
    /// 见 [`CounterUnit`]。默认 `Tokens`，与本字段引入前的行为一致。
    unit: CounterUnit,
}
```

`pre_commit` 改为转发，新增带单位的入口（97 行附近）：

```rust
    pub async fn pre_commit(
        &self,
        key: &str,
        limits: &RateLimit,
    ) -> Result<Reservation, RateLimitError> {
        self.pre_commit_with_unit(key, limits, CounterUnit::Tokens).await
    }

    /// 同 [`Self::pre_commit`]，但指定该层计数的单位。
    pub async fn pre_commit_with_unit(
        &self,
        key: &str,
        limits: &RateLimit,
        unit: CounterUnit,
    ) -> Result<Reservation, RateLimitError> {
        let member = self.next_member();
        self.store.acquire(key, limits, &member).await?;
        let renewal = limits.concurrency.and_then(|_| {
            self.store
                .concurrency_lease_renewal_interval()
                .and_then(|interval| {
                    spawn_lease_renewal(
                        Arc::clone(&self.store),
                        key.to_string(),
                        member.clone(),
                        interval,
                    )
                })
        });
        Ok(Reservation {
            store: Arc::clone(&self.store),
            key: key.to_string(),
            member,
            renewal,
            committed: false,
            unit,
        })
    }
```

`Reservation` 加一个按单位取值的方法，`MultiReservation` 加 `commit`：

```rust
impl Reservation {
    /// 该层应记的量：token 层记 token 数，花费层记 micro-USD。
    fn amount_for(&self, tokens: u64, spend_micro_usd: u64) -> u64 {
        match self.unit {
            CounterUnit::Tokens => tokens,
            CounterUnit::MicroUsd => spend_micro_usd,
        }
    }
}

impl MultiReservation {
    /// 提交实际用量，每层按自己的 [`CounterUnit`] 取对应的数字。
    pub async fn commit(self, tokens: u64, spend_micro_usd: u64) {
        for r in self.reservations {
            let amount = r.amount_for(tokens, spend_micro_usd);
            r.commit_tokens(amount).await;
        }
    }

    /// 等价于 `commit(tokens, 0)`。保留是因为 44 处既有调用点只关心 token，
    /// 且它们本来就拿不到花费数字——一次性改签名会把这个改动摊到十几个文件。
    pub async fn commit_tokens(self, tokens: u64) {
        self.commit(tokens, 0).await;
    }
}
```

在 `crates/aisix-ratelimit/src/lib.rs` 的 `pub use limiter::{...}` 里加上 `CounterUnit`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p aisix-ratelimit --lib` — Expected: PASS，含两个新测试
Run: `cargo build --workspace` — Expected: 成功（`commit_tokens` 签名未变，44 处调用点不受影响）

- [ ] **Step 5: 提交**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets 2>&1 | grep -c "^warning" # 应为 0
git add crates/aisix-ratelimit/
git commit -m "feat(ratelimit): 预留层携带计数单位，提交时按单位分派

花费和 token 是同一种量的两种单位。store 对此无感——FixedWindowCounter
不关心它数的是什么——所以分派放在 Reservation 层。pre_commit 与
commit_tokens 保留原签名，44 处既有调用点不受影响。"
```

---

### Task 2: `max_spend_micro_usd` 字段与 schema

**Files:**
- Modify: `crates/aisix-core/src/models/rate_limit_policy.rs`（`max_tokens` 在 255 行，紧随其后）
- Modify: `schemas/resources/rate_limit_policy.schema.json`（生成，不手改）
- Test: `crates/aisix-core/src/models/rate_limit_policy.rs` 的 `mod tests`

**Interfaces:**
- Consumes: 无
- Produces: `RateLimitPolicy::max_spend_micro_usd: Option<u64>`

- [ ] **Step 1: 写失败的测试**

```rust
/// 新字段可省略：既有策略文档必须原样加载，升级不改变任何人的配置含义。
#[test]
fn max_spend_is_optional_and_absent_by_default() {
    let json = r#"{"name":"p","scope":"api_key","window":"day","max_requests":10}"#;
    let p: RateLimitPolicy = serde_json::from_str(json).expect("既有文档仍可加载");
    assert_eq!(p.max_spend_micro_usd, None);
    // 未设置时不出现在序列化结果里，避免给存量文档凭空加字段。
    let back = serde_json::to_string(&p).unwrap();
    assert!(!back.contains("max_spend_micro_usd"), "未设置不应序列化: {back}");
}

/// 单位是 micro-USD 的整数：5 美元 = 5_000_000。
#[test]
fn max_spend_round_trips_as_micro_usd_integer() {
    let json = r#"{"name":"p","scope":"api_key","window":"day","max_spend_micro_usd":5000000}"#;
    let p: RateLimitPolicy = serde_json::from_str(json).expect("解析");
    assert_eq!(p.max_spend_micro_usd, Some(5_000_000));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p aisix-core --lib max_spend -- --nocapture`
Expected: FAIL，`no field max_spend_micro_usd on type RateLimitPolicy`

- [ ] **Step 3: 加字段**

在 `crates/aisix-core/src/models/rate_limit_policy.rs` 的 `max_tokens`（255 行）之后插入：

```rust
    /// Spend ceiling for this window, in micro-USD (1 USD = 1,000,000).
    ///
    /// Spend is computed from the dispatched model's configured price, so a
    /// model with no price contributes nothing to this counter — the policy
    /// then admits every request regardless of the ceiling. The gateway
    /// counts those requests on `aisix_budget_unpriced_requests_total` and
    /// logs them rather than failing them.
    ///
    /// Not honoured on a `second` window: spend, like tokens, is only known
    /// after the upstream answers, so a one-second ceiling would lag by about
    /// its own width. Configuring one is reported rather than silently
    /// ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_spend_micro_usd: Option<u64>,
```

- [ ] **Step 4: 跑测试并重新生成 schema**

Run: `cargo test -p aisix-core --lib max_spend` — Expected: PASS
Run: `cargo run -p aisix-core --bin dump-schema`
Run: `grep -c max_spend_micro_usd schemas/resources/rate_limit_policy.schema.json` — Expected: ≥ 1

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add crates/aisix-core/ schemas/resources/
git commit -m "feat(core): RateLimitPolicy 新增 max_spend_micro_usd

单位用 micro-USD 整数而非 USD 浮点：这是个跨请求累加的计数器，
f64 累加会漂移，且 Prometheus 侧的 spend series 本来就是这个单位。
字段可省略，存量策略文档原样加载。"
```

---

### Task 3: 窗口投影、花费桶、以及泛化的失效告警

**Files:**
- Modify: `crates/aisix-proxy/src/quota.rs`（投影在 300–345 行，`warn_inert_max_tokens_once` 在 262 行，桶键在 180 行）
- Test: `crates/aisix-proxy/src/quota.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `CounterUnit`；Task 2 的 `max_spend_micro_usd`
- Produces: `fn spend_bucket_key(scope: &str, scope_ref: &str, policy_entry_id: &str) -> String`；
  `fn warn_inert_dimension_once(policy_name: &str, window: &str, dimension: &str)`

- [ ] **Step 1: 写失败的测试**

```rust
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
        (PolicyWindow::Minute, (|r: &RateLimit| r.tpm) as fn(&RateLimit) -> Option<u64>),
        (PolicyWindow::Hour,   (|r: &RateLimit| r.tph) as fn(&RateLimit) -> Option<u64>),
        (PolicyWindow::Day,    (|r: &RateLimit| r.tpd) as fn(&RateLimit) -> Option<u64>),
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p aisix-proxy --lib quota::tests::spend -- --nocapture`
Expected: FAIL，`cannot find function spend_bucket_key` / `spend_limits_for`

- [ ] **Step 3: 实现**

在 `crates/aisix-proxy/src/quota.rs` 加：

```rust
/// 花费层的桶键。与 token 层的 `policy:` 前缀分开：同一策略的两个维度
/// 共用一个桶，token 数会把花费额度吃掉，且不会有任何报错。
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
```

把 `warn_inert_max_tokens_once`（262 行）泛化 —— 不要复制一份只改名字的函数，
两份 once-cell 会各自去重，加第三个维度时又要再抄：

```rust
/// 就 `(policy, window, dimension)` 警告一次：某个维度在该窗口下无法执行。
/// 策略在控制平面看起来是被接受的，所以静默会让它读起来像生效了。
fn warn_inert_dimension_once(policy_name: &str, window: &str, dimension: &str) {
    static SEEN: OnceLock<Mutex<HashSet<(String, String, String)>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let k = (policy_name.to_string(), window.to_string(), dimension.to_string());
    if seen.lock().insert(k) {
        tracing::warn!(
            policy = %policy_name,
            window = %window,
            dimension = %dimension,
            "{dimension} ignored: per-{window} counter not implemented; \
             the policy is accepted but this ceiling is not enforced",
        );
    }
}
```

把原 `max_tokens` 的调用点（316 行）改为 `warn_inert_dimension_once(&policy.name, "second", "max_tokens")`，
并在同一 `PolicyWindow::Second` 分支加：

```rust
            if policy.max_spend_micro_usd.is_some() {
                warn_inert_dimension_once(&policy.name, "second", "max_spend_micro_usd");
            }
```

删除原 `warn_inert_max_tokens_once` 函数。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p aisix-proxy --lib quota` — Expected: PASS（含既有的 inert 测试，可能需按新函数名调整断言）

- [ ] **Step 5: 提交**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets 2>&1 | grep -c "^warning"
git add crates/aisix-proxy/src/quota.rs
git commit -m "feat(proxy): 花费的窗口投影与独立桶命名空间

花费桶用 spend: 前缀与 token 桶分开——共用一个桶会让 token 数吃掉
花费额度且不报错。inert 告警从 max_tokens 专用泛化为按维度参数化，
避免第三个维度出现时再抄一遍 once-cell。"
```

---

### Task 4: 在准入与提交路径上接入花费层

**Files:**
- Modify: `crates/aisix-proxy/src/quota.rs`（`reserve_layers` 约 360 行、`match_policy_layer` 约 136 行）
- Modify: `crates/aisix-proxy/src/chat.rs`、`messages.rs`、`responses.rs`、
  `completions.rs`（全部 `commit_tokens` 调用点——见下方"覆盖四个文件"）
- Test: `crates/aisix-proxy/src/quota.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 1 `pre_commit_with_unit` / `MultiReservation::commit`；Task 3 `spend_bucket_key` / `spend_limits_for`
- Produces: `reserve_layers` 对每个带 `max_spend_micro_usd` 的匹配策略额外预留一个 `CounterUnit::MicroUsd` 层

- [ ] **Step 1: 写失败的测试**

```rust
/// 一个同时设了 max_tokens 与 max_spend 的策略要预留两层，
/// 且花费层带 MicroUsd 单位。只留一层会让其中一个上限静默失效。
#[tokio::test]
async fn a_policy_with_both_ceilings_reserves_a_token_layer_and_a_spend_layer() {
    let snap = snapshot_with_policy(RateLimitPolicy {
        name: "both".into(),
        scope: Some(PolicyScope::ApiKey),
        window: Some(PolicyWindow::Day),
        max_tokens: Some(1_000),
        max_spend_micro_usd: Some(5_000_000),
        ..Default::default()
    });
    let state = test_state(&snap);
    let auth = test_auth_key(&snap, "k1");

    let res = reserve_layers(&state, &snap, &auth, None, None)
        .await
        .expect("两层都应预留成功");

    let keys = res.keys();
    assert!(keys.iter().any(|k| k.starts_with("policy:")), "缺 token 层: {keys:?}");
    assert!(keys.iter().any(|k| k.starts_with("spend:")), "缺花费层: {keys:?}");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p aisix-proxy --lib a_policy_with_both_ceilings -- --nocapture`
Expected: FAIL，断言 "缺花费层"

- [ ] **Step 3: 实现**

在 `reserve_layers` 里，对每个匹配到的策略，在现有 token 层预留之后追加：

```rust
        // 花费层：与 token 层同一个策略、同一个窗口，但独立的桶和单位。
        // 两层都要过——任一超限即拒，和多层限流的语义一致。
        if let Some(max_spend) = policy.max_spend_micro_usd {
            let spend_limits = spend_limits_for(window, max_spend);
            // second 窗口投影为空，此时不预留（已在 Task 3 报警）。
            if spend_limits.tpm.is_some()
                || spend_limits.tph.is_some()
                || spend_limits.tpd.is_some()
            {
                let key = spend_bucket_key(scope.as_str(), &scope_ref, &policy_entry_id);
                reservations.push(
                    state
                        .limiter
                        .pre_commit_with_unit(&key, &spend_limits, CounterUnit::MicroUsd)
                        .await
                        // 用 reserve_layers 里既有的错误构造形状（quota.rs:478 的
                        // `ProxyError::PolicyRateLimit { .. }`），不要另造 helper。
                        // Task 7 会把花费层的这个错误换成 BudgetExceeded。
                        .map_err(|e| policy_layer_error(policy, e))?,
                );
            }
        }
```

**接线必须覆盖全部四个 handler 文件**，不是只有 chat。实测提交点分布：
`chat.rs` 13 处、`messages.rs` 2 处、`responses.rs` 2 处、`completions.rs` 6 处
（`grep -c "commit_tokens(" crates/aisix-proxy/src/<f>.rs`）。

只接 chat 会让 `/v1/messages` 与 `/v1/responses` 的花费完全不计，
且不产生任何报错——正是仓库反复点名的 handler 家族静默漂移。Task 8 的
e2e 会驱动这三条，漏接必然失败。

每个提交点的改法相同：把 `commit_tokens(total)` 换成带花费的形式，
花费取该请求已算出的 `cost_usd`（`usage_attr::request_cost_usd` 的返回值）：

```rust
    // USD → micro-USD。四舍五入而非截断：单次调用常在 1e-4 美元量级，
    // 一律截断会让花费系统性偏低。
    let spend_micro_usd = (cost_usd * 1_000_000.0).round().max(0.0) as u64;
    reservation.commit(total_tokens, spend_micro_usd).await;
```

拿不到 `cost_usd` 的提交点（例如缓存命中、错误路径）保持调用
`commit_tokens(n)` 不变——它等价于 `commit(n, 0)`，语义正确：
没有上游调用就没有花费。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p aisix-proxy --lib` — Expected: PASS
Run: `cargo build --workspace` — Expected: 成功

- [ ] **Step 5: 提交**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets 2>&1 | grep -c "^warning"
git add crates/aisix-proxy/
git commit -m "feat(proxy): 带 max_spend 的策略额外预留一个花费层

与 token 层同策略同窗口，独立桶与单位；任一超限即拒。
USD→micro-USD 用四舍五入而非截断：单次调用常在 1e-4 美元量级，
截断会让花费系统性偏低。"
```

---

### Task 5: 未定价模型的可见性

**Files:**
- Modify: `crates/aisix-obs/src/metrics.rs`（常量区约 42–53 行，`record_cache_event` 约 1053 行可作模板）
- Modify: `crates/aisix-proxy/src/quota.rs`
- Test: `crates/aisix-obs/src/metrics.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 4 的花费层
- Produces: `Metrics::record_budget_unpriced(&self, policy: &str, model: &str)`；
  常量 `M_BUDGET_UNPRICED_REQUESTS_TOTAL = "aisix_budget_unpriced_requests_total"`

- [ ] **Step 1: 写失败的测试**

```rust
/// 一个配了预算却调度到未定价模型的请求，必须在指标上留下痕迹。
/// 不留痕的话，"预算配了但从不触发"和"预算没被超过"在监控上完全一样。
///
/// 形状照抄同文件的 `recording_a_request_renders_in_exposition_format`。
#[test]
fn unpriced_request_under_a_budget_is_counted() {
    let m = Metrics::new(false);
    m.record_budget_unpriced("team-daily", "gpt-4o-mini");
    let rendered = m.render();
    assert!(
        rendered.contains(M_BUDGET_UNPRICED_REQUESTS_TOTAL),
        "series 缺失: {rendered}"
    );
    assert!(rendered.contains("policy=\"team-daily\""), "policy 标签缺失");
    assert!(rendered.contains("model=\"gpt-4o-mini\""), "model 标签缺失");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p aisix-obs --lib unpriced_request -- --nocapture`
Expected: FAIL，`no method named record_budget_unpriced`

- [ ] **Step 3: 实现**

在 `crates/aisix-obs/src/metrics.rs` 常量区加：

```rust
/// Requests admitted under a policy that sets a spend ceiling while the
/// dispatched model has no configured price, so the request contributes
/// nothing to the ceiling. Labels: `policy` (the policy's name), `model`
/// (the resolved row name, never a caller-supplied string).
///
/// A non-zero rate here means a budget is configured but not enforcing.
pub const M_BUDGET_UNPRICED_REQUESTS_TOTAL: &str = "aisix_budget_unpriced_requests_total";
```

照 `record_cache_event` 的形状加方法：

```rust
    /// Count one request that a spend ceiling could not price.
    pub fn record_budget_unpriced(&self, policy: &str, model: &str) {
        self.cached_counter(
            M_BUDGET_UNPRICED_REQUESTS_TOTAL,
            1,
            |k| {
                k.label(policy);
                k.label(model);
            },
            || {
                metrics::counter!(
                    M_BUDGET_UNPRICED_REQUESTS_TOTAL,
                    "policy" => policy.to_string(),
                    "model" => model.to_string(),
                )
            },
        );
    }
```

在 `quota.rs` 加一个专用的一次性告警——**不要**复用
`warn_inert_dimension_once`：那个函数的第二个参数是窗口名，传
`"unpriced-model"` 会渲染成 "per-unpriced-model counter not implemented"
这种读不通的话，而且去重维度也不对（这里要按模型去重，不是按窗口）。

```rust
/// 就 `(policy, model)` 警告一次：该模型没有配价，所以这条策略的花费
/// 上限对它不生效。按模型去重而不是按策略——一条策略可能命中很多模型，
/// 只报第一个会让其余的隐身。
fn warn_unpriced_model_once(policy_name: &str, model: &str) {
    static SEEN: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    if seen.lock().insert((policy_name.to_string(), model.to_string())) {
        tracing::warn!(
            policy = %policy_name,
            model = %model,
            "spend ceiling not enforced: this model has no configured price, \
             so requests to it contribute nothing to the ceiling",
        );
    }
}
```

在花费层预留处，模型未定价时记一笔并警告一次：

```rust
            // 未定价模型对花费计数器贡献 0，所以这个上限对它无效。
            // 放行，但不静默——拒绝会把一个配置疏漏变成流量中断。
            // 取已解析的模型行判断是否定价——不是请求里的模型名：
            // 通配符路径下两者不同，取错会让告警漏报或误报。
            let unpriced = snapshot
                .models
                .get_by_id(model_id)
                .map(|e| e.value.cost.is_none())
                .unwrap_or(true);
            if unpriced {
                // 标签必须用已解析的行名：通配符路径下调用方能自造模型名，
                // 直接打标签会让这个 series 基数无上限。
                let label = crate::usage_attr::metric_model_label(snapshot, model_name);
                state.metrics.record_budget_unpriced(&policy.name, &label);
                warn_unpriced_model_once(&policy.name, &label);
            }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p aisix-obs --lib && cargo test -p aisix-proxy --lib` — Expected: PASS

- [ ] **Step 5: 提交**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets 2>&1 | grep -c "^warning"
git add crates/aisix-obs/ crates/aisix-proxy/
git commit -m "feat(obs): 未定价模型下的预算失效有指标可见

配了预算却调度到无价模型时，花费恒为 0、上限永不触发。放行但计数：
不留痕的话，「预算配了但从不生效」和「预算没被超过」在监控上完全一样。
model 标签取已解析行名，避免通配符路径下基数失控。"
```

---

### Task 6: 删除 HTTP 预算客户端（破坏性变更）

**Files:**
- Delete: `crates/aisix-proxy/src/budget.rs`
- Create: `crates/aisix-proxy/src/budget_reason.rs`（只保留 `BudgetReason`）
- Modify: `crates/aisix-proxy/src/lib.rs`（模块声明）
- Modify: `crates/aisix-proxy/src/error.rs`（`BudgetReason` 的引用路径）
- Modify: `crates/aisix-server/src/main.rs`（约 703–715 行的 `BudgetClient` 装配）
- Modify: `crates/aisix-proxy/src/quota.rs`（删除 `check_budget` 的 HTTP 调用）

**Interfaces:**
- Consumes: 无
- Produces: `budget_reason::BudgetReason`（字段与原结构完全一致）

- [ ] **Step 1: 先确认现状为绿**

Run: `cargo test --workspace 2>&1 | grep -c "test result: FAILED"` — Expected: `0`

- [ ] **Step 2: 迁移 `BudgetReason`**

把 `budget.rs` 中的 `BudgetReason` 结构与 `impl`（含 `message_only`）整体移到新文件
`crates/aisix-proxy/src/budget_reason.rs`，模块头写明它现在由本地策略填充：

```rust
//! 预算错误的结构化原因。
//!
//! 这是对调用方的契约的一部分——`error.budget.*` 的形状——所以在预算从
//! 控制平面判定改为本地策略判定后仍然保留，只是换了填充者。
```

- [ ] **Step 3: 删除客户端与装配**

```bash
git rm crates/aisix-proxy/src/budget.rs
```

`lib.rs` 中 `mod budget;` 改为 `mod budget_reason;`。
`main.rs` 删除 `control_plane_base` / `BudgetClient::new(...)` 相关行。
`quota.rs` 的 `check_budget` 删除对 `state.budgets.check(...)` 的调用与 `ProxyState.budgets` 字段。

- [ ] **Step 4: 确认编译并全量测试**

Run: `cargo build --workspace` — Expected: 成功
Run: `cargo test --workspace 2>&1 | grep -c "test result: FAILED"` — Expected: `0`
Run: `grep -rn "budget_check\|BudgetClient" --include=*.rs crates/ | wc -l` — Expected: `0`

- [ ] **Step 5: 提交**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets 2>&1 | grep -c "^warning"
git add -A crates/
git commit -m "feat!: 删除控制平面预算 HTTP 客户端

BREAKING CHANGE: 预算不再通过 GET /dp/budget_check 判定，改由 etcd 中
带 max_spend_micro_usd 的 RateLimitPolicy 本地执行。未配置此类策略的
部署升级后将没有预算限制——迁移方式是把控制平面里的预算配置改写成
对应 scope 的策略资源。

BudgetReason 保留（移入 budget_reason.rs）：error.budget.* 是对调用方的
契约，不应因为服务端换了执行位置而变化。"
```

---

### Task 7: 从本地策略填充预算错误信封

**Files:**
- Modify: `crates/aisix-proxy/src/quota.rs`
- Test: `crates/aisix-proxy/src/quota.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 6 的 `budget_reason::BudgetReason`；Task 4 的花费层
- Produces: 花费层超限时返回 `ProxyError::BudgetExceeded`，而非通用的 `PolicyRateLimit`

- [ ] **Step 1: 写失败的测试**

```rust
/// 花费超限要报成预算错误，不是通用限流错误——两者状态码相同（429），
/// 但错误分类与 error.budget.* 字段不同，客户端据此区分"钱用完了"
/// 和"请求太快了"，这是两种完全不同的处置。
#[tokio::test]
async fn a_spend_ceiling_breach_reports_as_budget_not_rate_limit() {
    let snap = snapshot_with_policy(RateLimitPolicy {
        name: "tiny".into(),
        scope: Some(PolicyScope::ApiKey),
        window: Some(PolicyWindow::Day),
        max_spend_micro_usd: Some(1),   // 1 micro-USD，第二次必超
        ..Default::default()
    });
    let state = test_state(&snap);
    let auth = test_auth_key(&snap, "k1");

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
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p aisix-proxy --lib a_spend_ceiling_breach -- --nocapture`
Expected: FAIL，得到 `PolicyRateLimit` 而非 `BudgetExceeded`

- [ ] **Step 3: 实现**

花费层预留失败时，构造 `BudgetReason` 并返回 `BudgetExceeded`：

```rust
fn spend_exceeded_error(
    policy: &RateLimitPolicy,
    scope: &str,
    scope_ref: &str,
    max_spend_micro_usd: u64,
    retry_after_seconds: Option<u64>,
) -> ProxyError {
    let usd = |micro: u64| format!("{:.6}", micro as f64 / 1_000_000.0);
    ProxyError::BudgetExceeded(Box::new(BudgetReason {
        message: format!("spend ceiling reached for policy {}", policy.name),
        scope: Some(scope.to_string()),
        scope_ref: Some(scope_ref.to_string()),
        limit_usd: Some(usd(max_spend_micro_usd)),
        spent_usd: None, // 窗口计数器不回读当前值；上限与重试时间足以处置
        period: policy.window.map(|w| w.as_str().to_string()),
        period_resets_at: None,
        retry_after_seconds,
    }))
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p aisix-proxy --lib` — Expected: PASS

- [ ] **Step 5: 提交**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets 2>&1 | grep -c "^warning"
git add crates/aisix-proxy/src/quota.rs
git commit -m "feat(proxy): 花费超限报成预算错误而非通用限流错误

两者同为 429，但错误分类与 error.budget.* 不同——客户端据此区分
「钱用完了」和「请求太快了」，这是两种不同的处置。"
```

---

### Task 8: 端到端覆盖三个 endpoint 家族

**Files:**
- Create: `tests/e2e/src/cases/spend-budget-e2e.test.ts`

**Interfaces:**
- Consumes: Task 2–7 全部
- Produces: 无（终点任务）

- [ ] **Step 1: 写 e2e**

按 `tests/e2e/src/cases/group-model-condition-ratelimit-e2e.test.ts` 的结构写。
mock 上游每次返回固定 usage，模型配 `cost` 使每次调用恰好 1000 micro-USD；
策略设 `max_spend_micro_usd: 2500`，则第 3 次必被拒。

三个 endpoint 家族各驱动一遍：`/v1/chat/completions`、`/v1/messages`、`/v1/responses`。
断言：前两次 200，第三次 **429** 且 body 的 `error.type` 为 `billing_error`、
`error.budget.scope` 为 `api_key`。

另加一个 case：模型不配 `cost` 时请求全部放行，且 `/metrics` 中
`aisix_budget_unpriced_requests_total` 的增量 > 0。

- [ ] **Step 2: 跑，确认通过**

Run: `cd tests/e2e && npx vitest run src/cases/spend-budget-e2e.test.ts`
Expected: PASS

- [ ] **Step 3: RED 校验**

保存当前二进制，用 `git stash` 回到未实现状态构建一个对照二进制，
以 `AISIX_BIN=<对照>` 重跑此 spec：

Run: `AISIX_BIN=/tmp/aisix-prebudget npx vitest run src/cases/spend-budget-e2e.test.ts`
Expected: **FAIL** —— 第三次请求返回 200 而非 429（预算未执行）。
若它通过，说明这个 e2e 没有真正咬住行为，必须重写而不是接受。

- [ ] **Step 4: 全量回归**

Run: `cd tests/e2e && npx vitest run` — Expected: 全绿
Run: `cargo test --workspace 2>&1 | grep -c "test result: FAILED"` — Expected: `0`

- [ ] **Step 5: 提交**

```bash
git add tests/e2e/src/cases/spend-budget-e2e.test.ts
git commit -m "test(e2e): 花费预算在三个 endpoint 家族上的端到端断言

只驱动 chat 的 e2e 会永远绿着，而 Anthropic SDK 与 Codex 的流量
静默失效。三条路径各断言一次超限被拒，另断言未定价模型的可见性指标。
已对未实现的二进制做 RED 校验。"
```

---

## Self-Review

**Spec 覆盖检查**（逐节对照 `docs/design/2026-08-20-spend-budget-design.md`）：

| Spec 小节 | 实现于 |
| --- | --- |
| 数据模型 `max_spend_micro_usd` | Task 2 |
| 执行机制 / `CounterUnit` / `commit` 分派 | Task 1 |
| 花费桶命名空间 / 窗口投影 | Task 3 |
| `window: second` 泛化告警 | Task 3 |
| 准入与提交接线 | Task 4 |
| 未定价模型 metric + 告警 | Task 5 |
| 错误信封由本地填充 | Task 7 |
| 删除 budget.rs / 破坏性变更 | Task 6 |
| 测试（单元 + 三家族 e2e + RED） | Task 1–8 各自的测试步骤 + Task 8 |

无遗漏。

**自审改掉的两处**（记录下来，因为它们是"计划看起来合理但会让执行者卡住"的典型）：

1. Task 5 的测试原本调用 `Metrics::new_for_test()` / `render_prometheus()`——
   这两个方法**不存在**，是我照着"应该有"写的。已改为仓库真实使用的
   `Metrics::new(false)` / `.render()`，并指明可照抄的同文件既有测试。
2. Task 5 原本把 `"unpriced-model"` 当作*窗口名*传给
   `warn_inert_dimension_once`，会渲染出 "per-unpriced-model counter not
   implemented" 这种读不通的日志，且去重维度错误。已改为专用的
   `warn_unpriced_model_once`，按 `(policy, model)` 去重。

**已知偏差（有意为之，记录在此）**：

- `spent_usd` 在错误信封里留空。不是因为没有回读接口——`Limiter::peek`
  （`store/local.rs:263-287`，`RateLimitStatus`见 `limiter.rs:40-47`）确实
  存在，但它只读 rpm/tpm 这两个分钟窗口计数器，从不读 tph/tpd。对一条
  hour/day 窗口的花费策略调它，回读到的会是"当前这一分钟"的花费，冒充
  整个周期的花费——一个看起来权威、实际算错窗口的数字比留白更糟。要修
  `peek` 本身去认窗口，是 store 层的改动，本计划的核心约束是 store 不改。
  上限 + 重试时间对客户端处置已足够。若将来需要，那是独立的一次改动。
- 计划文件放在 `docs/design/` 而非技能默认的 `docs/superpowers/plans/`，
  与本项目的 spec 位置保持一致。
