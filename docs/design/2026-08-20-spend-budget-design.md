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
| `scope` / `scope_ref` | 策略的 `scope` / `scope_ref`，已有 |
| `limit_usd` | `max_spend_micro_usd`，micro-USD 换算回 USD 字符串 |
| `spent_usd` / `period_resets_at` | 留空——`Limiter::peek` 只读 rpm/tpm 这两个分钟窗口计数器，从不读 tph/tpd；对 hour/day 窗口的花费策略调它，回读到的会是"当前这一分钟"的花费，冒充整个周期的花费，一个权威外观、算错窗口的数字比留白更糟。要修 `peek` 本身去认窗口是 store 层改动，本设计的核心约束（store 层不改）不允许 |
| `period` | `PolicyWindow` |
| `retry_after_seconds` | `RateLimitError::retry_after_secs()` 已返回的秒数 |

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
- **jobs 面（files / batches / fine-tuning）的花费不计入预算**。
  `crates/aisix-proxy/src/jobs.rs` 的五个准入点——`907`、`1153`、`1219`、
  `1409`、`1594`——都写成 `let _reservation = quota::enforce(...)`，预留完就
  直接丢弃，从不提交。所以这些请求既不计 token，也不计花费：一条
  `scope: api_key` 的预算策略对批处理与微调的支出**完全无感**，且不会有任何报错。

  这是**有意延后**，不是遗漏。该面上 token 也同样不提交，只补花费会造成新的
  不一致（同一个预留里一个维度记、另一个维度不记）；而且批处理的账要在
  作业**完成**时才结得出来，那是一条与同步请求完全不同的时序，需要单独一次
  设计（`jobs.rs:1855` 已经能算出完成时的 `cost_usd`，缺的是把它接回预留，
  以及预留跨越作业生命周期的持有方式）。

  本仓库的 issue 已关闭，所以这里的记录就是唯一的记录：补这一面时，token 与
  花费必须一起接，并且要覆盖 batch 与 fine-tuning 两条。

- **虚拟父级（routing / ensemble / semantic）调度到的真正未定价目标不产生信号**。
  `aisix_budget_unpriced_requests_total` 只在直接模型（含 embedding）上断言：
  预留花费层的那一刻还不知道会调度到哪个具体目标，而虚拟父级这一行结构上
  永远不带 `cost`——对它断言"未定价"是没有依据的假信号，因为真实花费是按
  *调度到的目标* 定价的（`usage_attr::request_cost_usd` 吃的是实际调度目标
  的 model_id），完全可能非零。所以通过 Model Group / ensemble / 语义路由
  调度到一个确实没配价的目标时，这条 metric 什么都不会记录——这是已知盲区，
  不是 bug。一个假的"未生效"信号比沉默更糟：会教会运维忽略这条 series，
  代价比留白更高。

## 跟进项

- `set_budget_gauges`、`clear_budget_gauges`（`crates/aisix-obs/src/metrics.rs`）
  自本任务（Task 6，删除控制平面预算 HTTP 客户端）起没有生产调用方：它们
  唯一的数据来源是控制平面预算判定返回的 `Decision.budget: Option<BudgetDetails>`
  （`limit_usd`/`spent_usd`/`remaining_usd` 是 dollar f64，`reset_seconds`
  是 u64 秒数），这个来源已随 HTTP 客户端一起删除。两者已记入
  `crates/aisix-obs/tests/every_emit_has_a_caller.rs` 的 `ALLOWED_UNCALLED`，
  附带下面这条阻塞说明。
- 因此 `aisix_budget_details_present`、`aisix_budget_limit_usd`、
  `aisix_budget_spent_usd`、`aisix_budget_remaining_usd`、
  `aisix_budget_reset_seconds` 这五条 gauge 系列目前完全不再产生任何数据点——
  `/metrics` 上看不到它们（与仍然存活的计数器
  `aisix_budget_unpriced_requests_total` 无关，那条走的是未定价模型的另一条
  路径，不受本任务影响）。

  **为什么不顺手把它们接回本地花费层，而是留作独立跟进：**

  1.（主要阻塞）标签集不匹配。gauge 的标签是
     `BudgetLabels{api_key_id, team_id, user_id}`，而花费桶的键是
     `spend:{scope}:{scope_ref}:{policy_id}`（`quota.rs:426` 的
     `spend_bucket_key`）。一条 `scope: team` 的策略压根没有 `api_key_id`——
     它的花费桶从来不按 api_key 切分，所以不存在一种"保留原标签集"的方式
     把它读回 `BudgetLabels`。要接上，得先回答 gauge 该用什么标签集（桶的
     `scope`/`scope_ref`？还是仍按调用方的 api_key/team/user，即使策略本身
     按别的维度分桶？），这是一次独立的语义设计，不是接线活。
  2.（次要阻塞）就算标签集有了答案，读取现值意味着要在请求路径上对
     `Limiter::peek` 做一次额外查询——在 `RedisStore` 后端下，这正是本任务
     刚刚删掉的那种"每请求一次网络往返"，只是换了个目标而已。

  跟进任务需要：先定下 gauge 的标签集语义，再决定是从 `Limiter::peek` 同步
  读、还是异步/采样读；同时决定是否保留 dollar-f64 的对外形状，还是改成
  micro-USD u64（与本设计其余部分保持一致）。本仓库 issue 已关闭，所以这里
  的记录就是唯一的记录。

## 备注：本文件位置

本仓库的 `CLAUDE.md` 规定不在 `docs/` 下放散文档，理由是用户文档已外迁、
避免与文档站点漂移。该规矩针对的是**用户可见文档**；本文是内部设计文档，
不发布到任何文档站点。放在 `docs/design/` 下，与用户文档路径区分。
