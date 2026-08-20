# 花费预算：从控制平面同步调用改为本地策略执行

日期：2026-08-20
状态：待实现

## 为什么

网关当前的花费预算是**请求路径上的同步控制平面调用**。`quota::enforce` 在准入阶段调用
`check_budget`，后者查一个 5 秒 TTL 的 LRU，未命中就发一次
`GET {control_plane}/dp/budget_check?api_key_id=<uuid>` 并阻塞等待
（`crates/aisix-proxy/src/budget.rs`）。控制平面不可达时沿用上次决定最多 600 秒，
超时后按上次响应携带的 `fail_mode` 处理。

这有两个问题：

1. **数据面对控制面的运行时依赖。** 每个 api_key 每 5 秒就有一个请求要等一次网络往返。
   缓存和 sticky 降级是在给这个依赖打补丁，不是消除它。
2. **同一类判定走了两条路。** 限流（`rps/rpm/rph/rpd/tpm/tph/tpd/concurrency`）在本地执行，
   预算在远端执行。两者回答的是同一个问题——"这个 key 还能不能继续用"——却有完全不同的
   可用性特征和故障模式。这个不对称本身就是设计缺陷。

另有一个促成因素：本仓库不再依赖某个特定的商业控制平面实现，
所以"预算由控制平面下发"不再是一个可以假定成立的前提。

## 已定的决策

| 决策 | 选择 | 备注 |
| --- | --- | --- |
| 层级 | api_key + team 两层 | `PolicyScope` 已含 `ApiKey`/`Team`/`Model`/`Member`/`TeamMember` |
| 周期 | day / hour / minute，**不做 month** | 月不是固定秒数，需新的日历感知原语；用"日额度 ≈ 月预算/30"近似 |
| 未定价模型 | 放行 + 告警指标 + 日志 | 不断流，但不静默 |
| 承载方式 | `RateLimitPolicy` 的新维度 | 不新建资源类型 |
| 旧 HTTP 客户端 | 删除 | 见"破坏性变更" |

## 已核实的现状（实现依据）

以下都是读代码确认的，不是推断：

- `PolicyScope` 变体：`ApiKey` / `Model` / `Team` / `Member` / `TeamMember`
  （`crates/aisix-core/src/models/rate_limit_policy.rs`）。**team 层级无需新增任何东西。**
- `PolicyWindow` 变体：`Second` / `Minute` / `Hour` / `Day`。
- 策略通过投影到 7 字段的 `RateLimit` 来执行：`window: minute` 时
  `rl.rpm = max_requests; rl.tpm = max_tokens`，日/时同理
  （`crates/aisix-proxy/src/quota.rs`）。
- 预留桶键形如 `policy:{scope}:{scope_ref}:{policy_entry_id}`（`quota.rs:180`）。
- `FixedWindowCounter` 是 **epoch 对齐的固定窗口**：
  `bucket_start = (now_secs / window_secs) * window_secs`
  （`crates/aisix-ratelimit/src/window.rs`）。因此 86400 秒窗口在 **UTC 午夜**归零，
  日预算天然与自然日对齐。
- `RateStore` trait 有 `LocalStore` 与 `RedisStore` 两个实现
  （`crates/aisix-ratelimit/src/store/`）。**跨网关协调已经存在。**
- `KeyState` 持有 7 个 `FixedWindowCounter` 加一个 `in_flight`。
- `MultiReservation::commit_tokens(self, tokens: u64)` 把**同一个数字**提交给所有层
  （`crates/aisix-ratelimit/src/limiter.rs`）。
- `warn_inert_max_tokens_once` 已经确立了"某维度在某窗口下不生效就警告一次"的模式
  （`quota.rs:262`）。

## 数据模型

`RateLimitPolicy` 新增一个字段，其余不动：

```rust
/// 该窗口内允许的花费上限，单位 micro-USD（1 USD = 1_000_000）。
///
/// 用整数而不是 USD 浮点：这是个跨请求累加的计数器，f64 累加会漂移，
/// 而 Prometheus 侧的 `aisix_llm_spend_micro_usd_total` 本来就是这个单位。
pub max_spend_micro_usd: Option<u64>,
```

不新增资源类型，不新增 `ApiKey.budget` 内联字段。策略资源已能表达
"某 key 每日 5 美元"和"某 team 每日 100 美元"，再加一条内联路径只会制造第二个真相来源。

## 执行机制

**store 层不改一行。** `FixedWindowCounter` 不关心它数的是什么；token 和钱对它是同一种量。
改动集中在 reservation 层：

```rust
/// 一个预留层计数的单位。store 对此无感——只有 reservation 知道
/// 它那个桶里的数字是 token 还是钱。
enum CounterUnit { Tokens, MicroUsd }

struct Reservation { key: String, unit: CounterUnit, /* 其余不变 */ }

impl MultiReservation {
    /// 按每层的 unit 分派：token 层收 `tokens`，花费层收 `spend_micro_usd`。
    pub async fn commit(self, tokens: u64, spend_micro_usd: u64);

    /// 保留，等价于 `commit(tokens, 0)`。仍有大量调用点只关心 token，
    /// 一次性全部改签名会把这个改动摊到十几个文件上，且那些调用点
    /// 本来就拿不到花费数字。
    pub async fn commit_tokens(self, tokens: u64);
}
```

流式路径同理：`add_tokens_all(keys, tokens)` 旁边加
`add_spend_all(keys, spend_micro_usd)`，而不是改前者的签名——
它是同步的、且被 SSE 完成回调调用，签名变更会波及那条路径上的每个调用者。

花费桶使用独立命名空间，与 token 桶区分：

```
policy:{scope}:{scope_ref}:{policy_entry_id}    ← 现有，计 token / 请求数
spend:{scope}:{scope_ref}:{policy_entry_id}     ← 新增，计 micro-USD
```

窗口投影沿用现有规则：`day → tpd`、`hour → tph`、`minute → tpm`。

**时序与 token 完全一致**：准入时只检查不递增（花费和 token 一样，事后才知道），
响应完成后提交。

由此得到：

- 单网关：`LocalStore`，进程内，零网络调用
- 多网关：`RedisStore`，原子自增，**强一致**——且 Redis 是数据面自身的依赖，
  不是控制面往返

### `window: second` 下不生效

没有秒级 token 窗口，所以 `max_spend_micro_usd` 在 `window: second` 下无法执行。
把 `warn_inert_max_tokens_once` 泛化成 `warn_inert_dimension_once(policy, window, dimension)`，
两个维度共用，而不是复制一份只改名字的函数——两份 once-cell 会各自去重，
将来加第三个维度时又要再抄一遍。

选择警告而非在写入时拒绝，是**刻意与 `max_tokens` 保持一致**：同一个策略资源上，
两个同类维度在同一个窗口下应该有相同的失效语义，否则使用者要记两套规则。

## 未定价模型

花费由 `Model.cost` 算出。**没有配 `cost` 的模型，每次调用的花费恒为 0，
所以挂在它上面的预算永远不会触发。** 这正是仓库反复点名的
"accepted-but-unread config"（#962）与"never half-honored"（#963）那一类。

处理方式：放行请求，但让它可见。

- 新增 metric `aisix_budget_unpriced_requests_total{policy, model}`。
  `model` **必须**取 `usage_attr::metric_model_label` 的结果（已解析的行名），
  不能用调用方传入的字符串——通配符路径下调用方可以自造模型名，
  直接打标签会让这个 series 的基数无上限（#451 那条守则）
- 每个 `(policy, model)` 组合 warn 一次，复用已有的 once 机制避免刷日志
- 控制台模型页的定价列已有"未设"标记，补一句说明预算对该模型无效

选择放行而非拒绝，是因为拒绝会让一个**配置疏漏**（忘了填价）表现为**流量中断**，
而且中断的理由与用户的实际用量无关。可见性解决的是"静默"，不需要用断流来解决。

## 错误信封

`BudgetReason` 结构保留，改由本地策略填充：

| 字段 | 来源 |
| --- | --- |
| `scope` / `scope_ref` | 策略桶键，已有 |
| `limit_usd` / `spent_usd` | 窗口检查结果，micro-USD 换算回 USD 字符串 |
| `period` / `period_resets_at` | `PolicyWindow` 与窗口滚动时刻 |
| `retry_after_seconds` | `WindowCheck::Full` 已返回的 retry 秒数 |

超限返回 **429 Too Many Requests**，错误分类 `billing_error`——
与现有 `ProxyError::BudgetExceeded` 完全一致（`error.rs:360`、`error.rs:396`），
不引入新状态码。

**对调用方的 JSON 形状不变。** 这是整个改动里唯一保持兼容的对外契约，有意为之：
客户端解析预算错误的代码不应该因为服务端换了执行位置而失效。

## 破坏性变更

删除：

- `crates/aisix-proxy/src/budget.rs`（571 行）
- `main.rs` 中的 `BudgetClient` 装配
- `managed` 配置中与 budget_check 相关的项

**影响**：现有 managed 部署若依赖 `/dp/budget_check` 做预算执行，升级后预算改由
etcd 中的 `RateLimitPolicy` 决定。**未配置带 `max_spend_micro_usd` 的策略 = 无预算限制。**

这是真正的行为变更，不是重构。必须在 commit message 与发版说明中写明，
并给出迁移指引：把控制平面里的预算配置改写成对应 scope 的策略资源。

保留 `BudgetReason`——它是对外错误契约的一部分，只是换了填充者。

## 测试

单元测试：

- `CounterUnit` 分派：token 层与花费层各自收到正确的数字
- 窗口投影：day/hour/minute 各自落到 tpd/tph/tpm
- micro-USD 精度：累加不漂移
- `window: second` 下的 inert 警告确实发出

端到端（必须，按仓库规矩这类 metric/gate 只有 e2e 断言才算交付）：

- 配置带 `max_spend_micro_usd` 的策略 → 打真实流量 → 在预期的花费点被拒
- 断言 `/metrics` 中的 `aisix_budget_unpriced_requests_total`
- **覆盖每个 endpoint 家族**：`/v1/chat/completions`、`/v1/messages`、`/v1/responses`
  三条，流式与非流式各一。只驱动 chat 的 e2e 会永远绿着，
  而 Anthropic SDK 与 Codex 的流量静默失效
- 每条断言都做 RED 校验：对着未改动的二进制跑，确认它会失败

## 非目标

- **月度周期**。需要日历感知的窗口原语，会改动 `FixedWindowCounter` 的核心逻辑
  以及 Redis 侧的 TTL 计算。用日额度近似。
- **`ApiKey.budget` 内联字段**。策略资源已足够。
- **强制中心化预算权威**。本设计明确选择本地执行；需要全局强一致的部署应当
  使用 `RedisStore`，那已经提供跨网关的原子性。

## 备注：本文件位置

本仓库的 `CLAUDE.md` 规定不在 `docs/` 下放散文档，理由是用户文档已外迁、
避免与文档站点漂移。该规矩针对的是**用户可见文档**；本文是内部设计文档，
不发布到任何文档站点。放在 `docs/design/` 下，与用户文档路径区分。
