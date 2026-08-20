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
    /// 本来就拿不到花费数字。改名为 `commit_tokens_no_spend`（而不是
    /// 保留 `commit_tokens` 这个名字）：审查发现原名字在一个带花费层的
    /// 预留上悄悄记 0 元、没有任何信号，改名让每个调用点读起来都是一句
    /// 明确的断言——"这里没有花费数字要记"。
    pub async fn commit_tokens_no_spend(self, tokens: u64);
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

### 这是软上限：并发超调是设计的一部分

"准入只检查、完成才提交"直接推出一个必须写在明面上的后果：**上限约束的是已记录的
花费，不是在途的花费。** 上限被越过的那一刻，所有已经在途的请求都会跑完、也都会
记账。所以一个窗口的最终花费可以超出 `max_spend_micro_usd`，超出量最多是那一刻
并发在途请求的花费总和。

这跟后端无关。`RedisStore` 的自增是原子的、跨网关强一致——强一致的是**计数器**，
不是上限。check-then-commit 之间的缺口来自"价格只有在上游答完之后才知道"这个事实
本身，换任何 store 都还在。

不做预扣（准入时先按估算扣一笔、完成后找补）的理由：估算依赖 prompt token 的准确
预计数和对输出长度的猜测，两者都不可靠；猜高了会在远未到上限时就开始拒绝合法请求，
猜低了并不比现在更安全。用一个不准的数字换一个仍然不精确的上限，不划算。

约束超调靠的是准入时就递增的那些维度。`quota.rs` 对
`snap.rate_limit_policies.entries()` 是**全表遍历、逐条叠加**的（不是取第一条匹配），
所以两条都能和花费上限并存：

- 同 scope 同窗口的 `max_requests`——`LocalStore::acquire` 里注释明写
  "Request limits — checked AND incremented"，准入时就计数，是硬的。它给出的界是
  *窗口花费 ≤ max_requests × 单请求最高价*。约束的是速率，不是在途量。
- 另一条 conditional 形式策略的 `limits.concurrency`，配 `group_by: [api_key]`
  ——这条才直接压在途量，界是 *超调 ≤ concurrency × 单请求最高价*。注意
  conditional 形式没有 `scope`，它靠 `group_by` 分桶，所以不是"同 scope"，
  而是"每个 key 各自一个并发桶"。

`max_spend_micro_usd` 是 classic 形式的维度，`limits` 是 conditional 形式的，
同一行策略上写不了；要两者都要就写两条策略。

**明确不做**的是新增一个 `max_concurrency` 字段：conditional 形式的
`limits.concurrency` 已经是这件事，而新增一个用户可配字段又会连带要求控制平面那半边
（schema、Go 模型、表单、i18n），在这里是纯粹的重复建设。

以上必须出现在 `max_spend_micro_usd` 的字段文档里（面向用户的 API 参考由它渲染），
不能只留在设计文档里。

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

**兼容性是部分的，不是"JSON 形状不变"。** 状态码（429）、错误分类
（`billing_error`）、`limit_usd` / `period` / `retry_after_seconds` 这几个
字段的语义与旧的控制平面判定一致，客户端只依赖这几个字段的解析代码不受影响。
但 `spent_usd` 与 `period_resets_at` 不是"不变"——它们从"控制平面填的真实值"
变成了"本设计刻意留空"（原因见上表），且 `BudgetReason` 上两者都是
`skip_serializing_if`，留空意味着响应体里这两个键**直接消失**，不是变成
`null`。字段形状层面这是一个可观察的破坏性变化，已挪到下面"破坏性变更"一并
记录，不再声称"唯一保持兼容"。

## 破坏性变更

删除：

- `crates/aisix-proxy/src/budget.rs`（571 行）
- `main.rs` 中的 `BudgetClient` 装配
- `managed` 配置中与 budget_check 相关的项

**影响**：现有 managed 部署若依赖 `/dp/budget_check` 做预算执行，升级后预算改由
etcd 中的 `RateLimitPolicy` 决定。**未配置带 `max_spend_micro_usd` 的策略 = 无预算限制。**

这是真正的行为变更，不是重构。必须在 commit message 与发版说明中写明，
并给出迁移指引：把控制平面里的预算配置改写成对应 scope 的策略资源。

保留 `BudgetReason`——它是对外错误契约的一部分，只是换了填充者，但填充者
换了不代表形状和数据完全不变，下面两条都是可观察的破坏性变化：

- **`spent_usd` 与 `period_resets_at` 从"有值"变成"永远缺席"。** 旧的控制平面
  判定会填这两个字段；本地策略判定选择留空（原因见"错误信封"一节），而
  `BudgetErrorBody` 上两者都是 `skip_serializing_if`——留空不是变成 `json
  null`，是响应体里这两个键直接消失。解析这两个字段的客户端代码升级后会
  拿到"键不存在"而不是"键为 null"，两者在多数 JSON 客户端里处理路径不同。
- **五条 `aisix_budget_*` gauge 系列从 `/metrics` 消失。** `aisix_budget_details_
  present` / `aisix_budget_limit_usd` / `aisix_budget_spent_usd` /
  `aisix_budget_remaining_usd` / `aisix_budget_reset_seconds` 的唯一数据来源
  是旧控制平面判定的 `Decision.budget`，随 HTTP 客户端一起删除后没有任何
  调用方还在喂它们。抓取这些 series 的看板/告警升级后会看到它们停止更新，
  而不是报错——静默降级。完整的"为什么不顺手接回本地花费层"分析见下面
  "跟进项"一节。

## 控制平面

**本任务只做了数据面的一半。** 控制平面那一半还没有任何进展，这一节把欠的账
和后果都写清楚——本仓库 issue 已关闭，这一节就是唯一的记录，不会有 #编号
可以链接。

**(a) 还欠的控制平面工作，按 CLAUDE.md 的四层顺序：**

1. `cp-admin.yaml` 里 `RateLimitPolicy` 资源的 schema 加一个字段（当前的闭合
   校验器会拒绝任何 schema 没列出的字段——`max_spend_micro_usd` 现在写进
   etcd 的唯一途径是绕过控制平面直接写 etcd），以及重新生成的 Go 绑定。
2. 控制平面 `internal/cpapi/resources/` 下这个资源的 Go 类型模型、请求校验、
   etcd 投影。
3. dashboard 里对应的表单字段，加上 `messages/en.json` 与 `messages/
   zh.json` 的 i18n 词条。
4. 配套的跨平面测试：`e2e/cases/` 下的 CP↔DP Go 集成测试，以及 dashboard
   表单的 Playwright 测试。

**(b) 在这四层落地之前，托管部署没有任何花费预算执行手段。** 旧的
`/dp/budget_check` 路径已被 Task 6 删除；新的 `max_spend_micro_usd` 只能
通过 `resources.yaml` 或直接写 etcd 配置——托管部署的用户既没有 dashboard
入口，也没有能通过控制平面校验器的写入路径。也就是说：**一个托管部署从
旧版本升级到这个分支之后，预算执行会静默消失，且没有任何替代手段能重新
打开它**，直到上面四层都落地。这不是"功能暂时受限"，是"这条能力在托管
场景下完全不可达"，和 `AISIX-Cloud#873` 记录的模式同一类。

**(c) `max_spend_micro_usd`（`u64`，单位 micro-USD）这个字段是临时的，不是
定案。** 它是数据面先起的名字，本仓库的资源模型不是这个字段形状的权威来源——
按 CLAUDE.md「资源模型以 cp-admin.yaml 为准」的规则，一旦控制平面团队定下
这个能力在 `cp-admin.yaml` 里的最终名字与形状（可能是别的整数单位，也可能是
一个十进制的 `max_spend_usd`，或别的完全不同的表达），本仓库都要收敛过去。
收敛方式是给新字段名加 `#[serde(alias = "max_spend_micro_usd")]`，让这一版
写入的存量文档在别名窗口内继续能读，而不是不打招呼的硬改名——`model.rs` 的
renames 规则同样适用于这里。

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

- **ensemble 全程不计入预算——不是未定价盲区，是完全不提交花费**。
  `crates/aisix-proxy/src/chat.rs:4036`、`chat.rs:4060` 与
  `crates/aisix-proxy/src/ensemble.rs:193` 三处都只调用
  `MultiReservation::commit_tokens_no_spend`（提交 token，花费恒记 0）；
  流式 ensemble 更进一步，`reservation.token_keys()` / `judge_reservation.
  token_keys()` 只取 token 层的键（`chat.rs:3760`、`3765`），花费层的键
  从未被取用，花费桶终身停在 0。结果是：一条 `scope: api_key` 的花费
  策略对 panel + judge 的真实花费——无论面板成员和裁判模型是否配了
  price——**完全无感**，且不会有任何报错或告警。这与下一条"虚拟父级未
  定价信号缺失"是两回事：那一条是"配了价也测不出未定价"的可观测性盲区，
  这一条是"配了价、真花了钱、但一分钱都不会计入预算"的执行盲区——是货币
  管控上的一个旁路，不是可观测性的空白。按 CLAUDE.md 的既定方向，ensemble
  是一个整体未来设计要处理的实验性面，这里不做零敲碎打的修补；这条记录
  只是把这个已知旁路和它的具体位置钉在案，供将来那次设计一次性接上——
  接的时候要把 token 与花费一起接，覆盖非流式（`chat.rs:4036/4060`、
  `ensemble.rs:193`）与流式（`chat.rs:3760/3765`）两条路径。

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
