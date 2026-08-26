# 一期 · 身份与预付账本 — 实现计划

> **给执行者：** 必须配合 superpowers:subagent-driven-development（推荐）或
> superpowers:executing-plans 逐任务实施。步骤用 `- [ ]` 复选框跟踪。

**目标：** 让外部用户能注册登录、看到自己的余额与自己的用量；管理员能列出
用户并发放额度；余额耗尽时该用户的密钥被自动停用。

**架构：** 新增 `aisix-portal` 二进制，自带 SQLite 账本（用户、凭据、余额、
流水）。网关零改动。密钥在一期由管理员用现有控制台手工创建并填 `user_id`；
门户只读用量、只写账本，以及在余额归零时把密钥置 `disabled`。

**技术栈：** Rust / axum 0.8 / sqlx(sqlite) / argon2 / React + Vite（门户前端）

**Spec：** `docs/design/2026-08-26-tenant-accounts-design.md`

## 全局约束

逐条抄自 spec，每个任务的要求都隐含包含本节。

- **网关零改动。** 本期不得修改 `crates/aisix-proxy`、`aisix-ratelimit`、
  `aisix-core/src/models/`。改到这些即为越界，停下来报告。
- **计费服务永不在热路径上。** 网关不得在请求路径上调用门户。门户只读指标、
  只写配置文件。（spec §3.3）
- **租户隔离是端点形状，不是过滤器。** 门户不暴露 PromQL。任何按用户维度返回
  数据的端点，`user_id` **只从会话取，永不从请求体取**。（spec §5.2）
- **口令用 argon2，密钥散列用 sha256。** 前者已在 `crates/aisix-console`
  依赖中；后者对高熵随机串是合适的。不得用 sha256 存口令。
- **余额变更必须在单个事务里。** 充值与扣减竞态不能丢钱。（spec §4.2）
- **明文凭据只出现一次。** 落库只存散列。
- **注释用中文，函数必须有类型标注，禁止裸 except / unwrap 在错误路径。**
- **一期已知限制，须在代码注释中写明，不得假装没有：** 对账读 Prometheus
  会有漂移（spec §3.1）；超支上界 = 轮询周期内消费 + 在途（spec §3.2）；
  会话存在进程内存里，门户重启即全体登出；门户要写 `resources.yaml`，
  因此必须与控制台同机或共享卷（裁决 2 的部署代价）；`web-portal/` 会重复
  `web/` 的一整套脚手架，这是 spec §5.1「分进程不分角色」换来的，接受。

## 一期裁决（spec 未定，此处定死）

**裁决 1 — 数据库用 sqlx + SQLite。**
工作区目前没有任何数据库依赖。选 sqlx 而非 rusqlite 是因为它是异步的（与
axum/tokio 一致，不必 `spawn_blocking`），且换 Postgres 只需改 feature 与连接
串——spec §8 把多实例门户列为未定，sqlx 让那个决定不必重写数据层。
**用运行时查询（`sqlx::query`），不用编译期宏**，避免构建时依赖 `DATABASE_URL`。

**裁决 2 — 停用密钥在一期走 `resources.yaml`。**
Admin API 是只读的，写接口是二期的事（spec §2 缺口 4、§7 二期）。一期的密钥
由管理员手工创建、数量少，停用事件稀有，因此门户用与控制台**同一套内容散列
乐观并发**写该文件，撞版本就重读重试。
**代价写进代码注释**：两个进程写同一个文件，不可扩展；二期换成 Admin API 写入。

**裁决 3 — 管理端 API 归门户，不归控制台。**
用户库归门户所有，只有一个写入者。控制台（管理界面）通过门户的管理端 API
访问，该 API 由独立于用户会话的管理凭据（`PORTAL_ADMIN_TOKEN`）保护。

**裁决 4 — 一期不做邮箱验证流程。**
预付模型下陌生人拿不到免费推理，验证的紧迫性低。`users.email_verified_at`
字段建好留空，流程二期或四期补。**不得**因此在注册处放松邮箱唯一性。

## 文件结构

| 文件 | 职责 |
|---|---|
| `crates/aisix-portal/Cargo.toml` | 新 crate。不进 workspace deps，与 console 同样自带版本 |
| `crates/aisix-portal/migrations/0001_init.sql` | `users` / `ledger` / `consumption_mark` 三张表 |
| `crates/aisix-portal/src/main.rs` | 进程启动、路由装配、配置读取 |
| `crates/aisix-portal/src/store.rs` | 数据库访问。**唯一**碰 SQL 的地方 |
| `crates/aisix-portal/src/auth.rs` | 注册、登录、会话 |
| `crates/aisix-portal/src/ledger.rs` | 余额与流水，事务边界在此 |
| `crates/aisix-portal/src/usage.rs` | 具名用量查询，`user_id` 由调用方注入 |
| `crates/aisix-portal/src/admin.rs` | 管理端 API（列用户、发放额度） |
| `crates/aisix-portal/src/sweeper.rs` | 控制环：轮询消费 → 扣减 → 归零停用 |
| `web-portal/` | 门户前端（React + Vite），与 `web/` 分离 |
| `crates/aisix-portal/tests/` | 集成测试 |
| `web-portal/e2e/` | Playwright E2E，跑真后端 |

---

### Task 1：crate 骨架与库表

**Files:**
- Create: `crates/aisix-portal/Cargo.toml`
- Create: `crates/aisix-portal/migrations/0001_init.sql`
- Create: `crates/aisix-portal/src/store.rs`
- Create: `crates/aisix-portal/src/main.rs`
- Modify: `Cargo.toml`（workspace members 增加该 crate）

**Interfaces:**
- Produces: `Store::open(path) -> Result<Store>`、`Store::pool() -> &SqlitePool`

- [ ] **Step 1：写建表 SQL**

```sql
-- 用户。email 唯一：注册处依赖数据库层的唯一约束，不靠先查后插（那有竞态）。
CREATE TABLE users (
  id              TEXT PRIMARY KEY,          -- uuid v4，即密钥上的 user_id
  email           TEXT NOT NULL UNIQUE,
  password_hash   TEXT NOT NULL,             -- argon2 PHC 串
  display_name    TEXT,
  email_verified_at TEXT,                    -- 一期恒为 NULL，见裁决 4
  disabled        INTEGER NOT NULL DEFAULT 0,
  created_at      TEXT NOT NULL
);

-- 流水。只追加，绝不 UPDATE/DELETE：余额是它的和，账要能重算。
CREATE TABLE ledger (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id     TEXT NOT NULL REFERENCES users(id),
  -- 正数入账（发放/充值），负数出账（消费）。单位 micro-USD，整数。
  -- 用整数是因为浮点做钱会累积误差，而这个产品的花费到千分之一美分。
  delta_micro_usd INTEGER NOT NULL,
  source      TEXT NOT NULL,                 -- admin_grant | consumption | payment
  note        TEXT,
  created_at  TEXT NOT NULL
);
CREATE INDEX ledger_user ON ledger(user_id, id);

-- 消费对账的水位线：记「已经计到哪个时刻」，不是「已经计了多少」。
--
-- 这里曾经想记累计额、每轮扣差值 —— 那是错的。花费指标是 counter，而且
-- **每个网关副本各自暴露一份**；任一副本重启都会让 `sum` 下陷，看起来就像
-- counter 重置。按累计额做差就会把水位线重新对齐到低点，那一刻起未入账的
-- 消费永久丢失，且毫无信号：用户白得推理，账面看不出异常。
--
-- 改成记时刻后，每轮查 `increase(...[自上次至今])`。`increase()` 是逐时间
-- 序列处理重置再求和的，跨副本天然安全，也没有缺口或重叠。
CREATE TABLE consumption_mark (
  user_id         TEXT PRIMARY KEY REFERENCES users(id),
  counted_through TEXT NOT NULL,           -- RFC3339，已计入流水的截止时刻
  updated_at      TEXT NOT NULL
);
```

- [ ] **Step 2：写失败的测试**

```rust
#[tokio::test]
async fn 建表后可以插入并读回一个用户() {
    let store = Store::open_memory().await.unwrap();
    store.insert_user("u1", "a@b.c", "hash", None).await.unwrap();
    let u = store.user_by_email("a@b.c").await.unwrap().unwrap();
    assert_eq!(u.id, "u1");
}

#[tokio::test]
async fn 同一邮箱不能注册两次() {
    let store = Store::open_memory().await.unwrap();
    store.insert_user("u1", "a@b.c", "h", None).await.unwrap();
    // 唯一约束必须由数据库拒绝，而不是靠调用方先查后插。
    assert!(store.insert_user("u2", "a@b.c", "h", None).await.is_err());
}
```

- [ ] **Step 3：跑测试确认失败**

`cargo test -p aisix-portal` → 编译失败（`Store` 不存在）

- [ ] **Step 4：实现 `Store`**，`open` / `open_memory` 应用 migrations

- 连接串须设 `busy_timeout`（建议 5s）。并发写入撞 `SQLITE_BUSY` 是常态，
  不设就是一条 flaky 测试 —— 而 flaky 不许靠放宽断言收场。

**订正（实施时实测推翻了审计的一条结论）：** 审计曾断言 `open_memory` 必须
自己拼 `cache=shared`，否则每条池连接拿到各自独立的库。**在 sqlx 0.8 上这是
错的**——探针实测 `sqlite::memory:` 下同一个池的多条连接共享同一个库
（`池 size=2`、另一条连接能读到）、不同池之间隔离。那条结论来自一般经验而
未对着本仓库的版本验证，绕路代码已删。留一条守卫测试盯住「同池共享」这个
Task 4 依赖的性质。
- [ ] **Step 5：跑测试确认通过**
- [ ] **Step 6：提交** `feat(portal): user and ledger schema`

---

### Task 2：注册与凭据

**Files:**
- Create: `crates/aisix-portal/src/auth.rs`
- Modify: `crates/aisix-portal/src/main.rs`（挂路由）

**Interfaces:**
- Consumes: `Store`（Task 1）
- Produces: `POST /api/register {email, password} -> 201 {user_id}`

- [ ] **Step 1：写失败的测试**

```rust
#[tokio::test]
async fn 口令以_argon2_落库_绝不明文() {
    let app = test_app().await;
    let r = post(&app, "/api/register",
        json!({"email":"a@b.c","password":"correct horse battery"})).await;
    assert_eq!(r.status, 201);

    let row = app.store.user_by_email("a@b.c").await.unwrap().unwrap();
    // 两条都要断：不含明文，且确实是 argon2 而不是别的什么散列。
    assert!(!row.password_hash.contains("correct horse"));
    assert!(row.password_hash.starts_with("$argon2"));
}

#[tokio::test]
async fn 重复邮箱返回_409_而不是_500() {
    let app = test_app().await;
    post(&app, "/api/register", json!({"email":"a@b.c","password":"xxxxxxxxxxxx"})).await;
    let r = post(&app, "/api/register", json!({"email":"a@b.c","password":"yyyyyyyyyyyy"})).await;
    // 唯一约束冲突是可预期的用户错误，不是服务端故障。
    assert_eq!(r.status, 409);
}

#[tokio::test]
async fn 过短的口令被拒() {
    let app = test_app().await;
    let r = post(&app, "/api/register", json!({"email":"a@b.c","password":"short"})).await;
    assert_eq!(r.status, 400);
}
```

- [ ] **Step 2：跑测试确认失败**
- [ ] **Step 3：实现**。口令最短 12 字符；argon2 用默认参数；唯一约束冲突映射为 409
- [ ] **Step 4：跑测试确认通过**
- [ ] **Step 5：提交** `feat(portal): registration with argon2 credentials`

---

### Task 3：登录与会话

**Files:**
- Modify: `crates/aisix-portal/src/auth.rs`

**Interfaces:**
- Produces: `POST /api/login`、`POST /api/logout`、`GET /api/session`；
  会话 cookie 形状与 `crates/aisix-console/src/main.rs` 一致（`Secure`、
  `HttpOnly`、`SameSite=Strict`）

- [ ] **Step 1：写失败的测试**

```rust
#[tokio::test]
async fn 口令错误不泄漏账号是否存在() {
    let app = test_app().await;
    register(&app, "a@b.c", "correct horse battery").await;
    let bad_pw = post(&app, "/api/login", json!({"email":"a@b.c","password":"wrong wrong wrong"})).await;
    let no_user = post(&app, "/api/login", json!({"email":"nobody@b.c","password":"wrong wrong wrong"})).await;
    // 两者必须无法区分，否则登录接口成了账号枚举器。
    assert_eq!(bad_pw.status, no_user.status);
    assert_eq!(bad_pw.body_text, no_user.body_text);
}

#[tokio::test]
async fn 未登录时会话接口不返回任何用户信息() {
    let app = test_app().await;
    let r = get(&app, "/api/session").await;
    assert_eq!(r.json["authed"], json!(false));
    assert!(r.body_text.find("@").is_none());
}
```

- [ ] **Step 2：跑测试确认失败**
- [ ] **Step 3：实现**。口令不匹配与账号不存在走同一条返回路径
- [ ] **Step 4：跑测试确认通过**
- [ ] **Step 5：提交** `feat(portal): login and session`

---

### Task 4：账本（事务边界）

**Files:**
- Create: `crates/aisix-portal/src/ledger.rs`

**Interfaces:**
- Produces: `Ledger::credit(user, micro, source, note)`、
  `Ledger::debit(user, micro, source, note)`、`Ledger::balance(user) -> i64`

- [ ] **Step 1：写失败的测试**

```rust
#[tokio::test]
async fn 余额是流水的和() {
    let l = test_ledger().await;
    l.credit("u1", 5_000_000, "admin_grant", None).await.unwrap();
    l.debit("u1", 1_500_000, "consumption", None).await.unwrap();
    assert_eq!(l.balance("u1").await.unwrap(), 3_500_000);
}

#[tokio::test]
async fn 并发的充值与扣减不丢钱() {
    let l = test_ledger().await;
    l.credit("u1", 1_000_000, "admin_grant", None).await.unwrap();

    // 50 笔入账与 50 笔出账同时打进去。任何一笔丢失或重复，
    // 最终余额都对不上——这正是「余额必须在事务里」要防的事。
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..50 {
        let a = l.clone();
        set.spawn(async move { a.credit("u1", 1_000, "admin_grant", None).await });
        let b = l.clone();
        set.spawn(async move { b.debit("u1", 400, "consumption", None).await });
    }
    while let Some(r) = set.join_next().await { r.unwrap().unwrap(); }

    assert_eq!(l.balance("u1").await.unwrap(), 1_000_000 + 50 * 1_000 - 50 * 400);
}

#[tokio::test]
async fn 扣到负数仍然入账_不能因余额不足而丢弃这笔消费() {
    let l = test_ledger().await;
    l.credit("u1", 1_000, "admin_grant", None).await.unwrap();
    // 消费已经发生了，钱已经花出去了。按直觉写成「余额不足则拒绝」，
    // 这笔就永远不入账 —— 又是一次静默白送。超支是被接受的（spec §3.2），
    // 账本必须如实记到负数，由控制环去停用密钥。
    l.debit("u1", 2_500, "consumption", None).await.unwrap();
    assert_eq!(l.balance("u1").await.unwrap(), -1_500);
}

#[tokio::test]
async fn 流水只追加_扣减不改写既有行() {
    let l = test_ledger().await;
    l.credit("u1", 1_000, "admin_grant", None).await.unwrap();
    let before = l.entries("u1").await.unwrap();
    l.debit("u1", 400, "consumption", None).await.unwrap();
    let after = l.entries("u1").await.unwrap();
    // 账要能重算，历史行不得被动过。
    assert_eq!(&after[..before.len()], &before[..]);
}
```

- [ ] **Step 2：跑测试确认失败**
- [ ] **Step 3：实现**。`balance` 用 `SUM(delta_micro_usd)`；写入在
      `BEGIN IMMEDIATE` 事务内
- [ ] **Step 4：跑测试确认通过**
- [ ] **Step 5：提交** `feat(portal): append-only balance ledger`

---

### Task 5：管理端 API

**Files:**
- Create: `crates/aisix-portal/src/admin.rs`

**Interfaces:**
- Produces: `GET /admin/users`、`POST /admin/users/:id/grant {micro_usd, note}`；
  以 `PORTAL_ADMIN_TOKEN` 保护（裁决 3）

- [ ] **Step 1：写失败的测试**

```rust
#[tokio::test]
async fn 用户会话打不开管理端() {
    let app = test_app().await;
    let s = register_and_login(&app, "a@b.c").await;
    // 带着合法的**用户**会话去敲管理端，必须 401/403。
    // 这条是裁决 3 的全部意义：两套凭据，互不通用。
    let r = get_with_session(&app, "/admin/users", &s).await;
    assert!(r.status == 401 || r.status == 403);
}

#[tokio::test]
async fn 发放额度会落成一条可审计的流水() {
    let app = test_app().await;
    let id = register(&app, "a@b.c", "correct horse battery").await;
    post_admin(&app, &format!("/admin/users/{id}/grant"),
        json!({"micro_usd": 5_000_000, "note": "首充赠送"})).await;

    let entries = app.ledger.entries(&id).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].source, "admin_grant");
    assert_eq!(entries[0].note.as_deref(), Some("首充赠送"));
}

#[tokio::test]
async fn 发放对象只能来自用户列表_不接受手输的任意标识() {
    let app = test_app().await;
    // 一期密钥由管理员手工创建并填 user_id。手输一个 uuid 填错一个字符，
    // 网关照常放行、指标打的是错标签、门户查不到用量 —— 于是**永不扣款**，
    // 用户免费用而没人会发现。发放端因此只接受已存在的用户 id。
    let r = post_admin(&app, "/admin/users/not-a-real-user/grant",
        json!({"micro_usd": 1_000_000})).await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn 发放负数被拒_而不是变成扣款() {
    let app = test_app().await;
    let id = register(&app, "a@b.c", "correct horse battery").await;
    let r = post_admin(&app, &format!("/admin/users/{id}/grant"),
        json!({"micro_usd": -1_000_000})).await;
    assert_eq!(r.status, 400);
}
```

- [ ] **Step 2：跑测试确认失败**
- [ ] **Step 3：实现**。`GET /admin/users` 返回列表供管理界面**选择**；
      发放端校验 user 存在，否则 404
- [ ] **Step 4：跑测试确认通过**
- [ ] **Step 5：提交** `feat(portal): admin user listing and quota grants`

---

### Task 6：租户隔离的用量查询

**Files:**
- Create: `crates/aisix-portal/src/usage.rs`

**Interfaces:**
- Produces: `GET /api/usage?range_hours=N` —— 返回**当前会话用户**的请求数、
  token 数、花费；`user_id` 由会话注入

- [ ] **Step 1：写失败的测试**

```rust
#[tokio::test]
async fn 端点不接受调用方提供的查询() {
    let app = test_app().await;
    let s = register_and_login(&app, "a@b.c").await;
    // 任何形式的自带查询都不该被接受——租户隔离是端点形状，不是过滤器。
    for probe in [
        "/api/usage?query=sum(aisix_llm_spend_micro_usd_total)",
        "/api/usage?user_id=someone-else",
    ] {
        let r = get_with_session(&app, probe, &s).await;
        assert!(r.status == 400 || !r.body_text.contains("someone-else"));
    }
}

#[tokio::test]
async fn 发往_prometheus_的查询里带的是会话用户的_id() {
    let app = test_app_with_fake_prom().await;
    let s = register_and_login(&app, "a@b.c").await;
    let uid = app.user_id("a@b.c").await;
    get_with_session(&app, "/api/usage?range_hours=24", &s).await;

    let q = app.prom.last_query();
    assert!(q.contains(&format!(r#"user_id="{uid}""#)));
}
```

- [ ] **Step 2：跑测试确认失败**
- [ ] **Step 3：实现**。查询模板写死在服务端，只有 `user_id` 与
      `range_hours` 是参数；`user_id` 须做 PromQL 标签值转义

- [ ] **Step 4：把「没有密钥绑到我」变成可见状态**

```rust
#[tokio::test]
async fn 没有密钥携带本人_user_id_时明确报出未绑定() {
    let app = test_app_with_config_without_my_key().await;
    let s = register_and_login(&app, "a@b.c").await;
    let r = get_with_session(&app, "/api/usage?range_hours=24", &s).await;
    // 这是 user_id 填错时唯一能被人看见的地方。没有这条，管理员打错一个
    // 字符的后果就是「用量一直是 0」—— 跟「还没开始用」在屏幕上没有区别，
    // 而它实际意味着这个人在免费用。
    assert_eq!(r.json["linked_keys"], json!(0));
    assert!(r.body_text.contains("未绑定"));
}
```
- [ ] **Step 5：跑测试确认通过**
- [ ] **Step 6：提交** `feat(portal): tenant-scoped usage queries`

---

### Task 7：控制环

**Files:**
- Create: `crates/aisix-portal/src/sweeper.rs`

**Interfaces:**
- Consumes: `Ledger`（Task 4）、`usage`（Task 6）
- Produces: `Sweeper::tick()` —— 一轮对账，供测试直接调用而不必等定时器

- [ ] **Step 1：写失败的测试**

```rust
#[tokio::test]
async fn 按时间窗查增量_而不是在累计值上做差() {
    let s = test_sweeper().await;
    s.ledger.credit("u1", 5_000_000, "admin_grant", None).await.unwrap();

    // 夹具按「时间窗 → 增量」应答，正是 increase() 的语义。
    s.prom.set_increase("u1", 1_000_000);
    s.tick().await.unwrap();
    assert_eq!(s.ledger.balance("u1").await.unwrap(), 4_000_000);

    // 这一轮窗口内没有新增量，不该再扣。
    s.prom.set_increase("u1", 0);
    s.tick().await.unwrap();
    assert_eq!(s.ledger.balance("u1").await.unwrap(), 4_000_000);
}

#[tokio::test]
async fn 查询窗口从水位线接到当前_不留缺口也不重叠() {
    let s = test_sweeper().await;
    s.ledger.credit("u1", 5_000_000, "admin_grant", None).await.unwrap();

    s.clock.set("2026-08-26T10:00:00Z");
    s.tick().await.unwrap();
    // 停摆 10 分钟后再跑一轮：窗口必须覆盖整段空档，
    // 否则停摆期间的消费就永远不入账了。
    s.clock.set("2026-08-26T10:10:00Z");
    s.tick().await.unwrap();

    let w = s.prom.last_window();
    assert_eq!(w.from, "2026-08-26T10:00:00Z");
    assert_eq!(w.to,   "2026-08-26T10:10:00Z");
}

#[tokio::test]
async fn 副本重启不会让这轮少扣钱() {
    // 花费指标是 counter，每个网关副本各自一份。以前的写法在累计值上做差，
    // 任一副本重启导致 sum 下陷时会被当成「重置」而重新对齐水位线，
    // 未入账的消费就永久丢了 —— 静默、且方向永远对用户有利。
    //
    // increase() 是逐序列处理重置后再求和的，所以副本重启对这一轮的
    // 增量没有影响。这条测试钉的就是「不再有重新对齐这回事」。
    let s = test_sweeper().await;
    s.ledger.credit("u1", 5_000_000, "admin_grant", None).await.unwrap();
    s.prom.set_increase("u1", 800_000);
    s.prom.simulate_replica_restart();      // 其中一个副本的 counter 归零
    s.tick().await.unwrap();
    assert_eq!(s.ledger.balance("u1").await.unwrap(), 4_200_000);
    // 水位线只会前进。
    assert!(s.counted_through("u1") > s.clock.previous());
}

#[tokio::test]
async fn 读取失败时水位线不前进() {
    let s = test_sweeper().await;
    s.ledger.credit("u1", 5_000_000, "admin_grant", None).await.unwrap();
    s.clock.set("2026-08-26T10:00:00Z");
    s.tick().await.unwrap();
    let mark = s.counted_through("u1");

    // Prometheus 读不到就必须原地不动。若此时把水位线推到当前时刻，
    // 这段时间的消费就被跳过了 —— 又是一次静默白送。
    s.prom.fail_next();
    s.clock.set("2026-08-26T10:05:00Z");
    assert!(s.tick().await.is_err());
    assert_eq!(s.counted_through("u1"), mark);
}

#[tokio::test]
async fn 余额归零会把该用户的密钥置_disabled() {
    let s = test_sweeper_with_config().await;   // 配置里有 user_id=u1 与 u2 两把密钥
    s.ledger.credit("u1", 1_000_000, "admin_grant", None).await.unwrap();
    s.prom.set_increase("u1", 1_200_000);
    s.tick().await.unwrap();

    assert!(s.ledger.balance("u1").await.unwrap() < 0);
    let doc = s.read_resources();
    assert_eq!(doc.key_with_user("u1").disabled, true);
    // 别人的密钥不许被碰。
    assert_eq!(doc.key_with_user("u2").disabled, false);
}

#[tokio::test]
async fn 补上余额后密钥被重新启用() {
    let s = test_sweeper_with_config().await;
    s.ledger.credit("u1", 1_000_000, "admin_grant", None).await.unwrap();
    s.prom.set_increase("u1", 1_200_000);
    s.tick().await.unwrap();
    assert_eq!(s.read_resources().key_with_user("u1").disabled, true);

    s.ledger.credit("u1", 5_000_000, "admin_grant", None).await.unwrap();
    s.prom.set_increase("u1", 0);
    s.tick().await.unwrap();
    assert_eq!(s.read_resources().key_with_user("u1").disabled, false);
}
```

- [ ] **Step 2：跑测试确认失败**
- [ ] **Step 3：实现**。每轮按
      `sum(increase(aisix_llm_spend_micro_usd_total{user_id="X"}[<counted_through→now>]))`
      取增量；**水位线只在入账成功后前进，读取失败原地不动**；写
      `resources.yaml` 用与控制台相同的内容散列乐观并发，撞版本重读重试
      （裁决 2）；轮询周期默认 15 秒（spec §3.2）
- [ ] **Step 4：跑测试确认通过**
- [ ] **Step 5：提交** `feat(portal): reconciliation sweeper`

---

### Task 8：门户前端

**Files:**
- Create: `web-portal/`（结构对照 `web/`：vite.config.ts、src/lib/api.ts、
  src/App.tsx、src/styles.css）

四个界面：注册、登录、我的余额、我的用量。**没有配置编辑，没有密钥管理**
（密钥是二期）。

- [ ] **Step 1：搭骨架并接通 `/api/session`**
- [ ] **Step 2：注册与登录界面**
- [ ] **Step 3：余额与流水界面**
- [ ] **Step 4：用量界面**（调 Task 6 的端点）
- [ ] **Step 5：`npm run build` 与 `tsc --noEmit` 通过**
- [ ] **Step 6：提交** `feat(portal): self-service portal frontend`

---

### Task 9：E2E（两层，都跑真后端）

**Files:**
- Create: `tests/e2e/src/cases/portal-quota.spec.ts`
- Create: `web-portal/e2e/fixture.ts`、`web-portal/e2e/portal.spec.ts`

计划初稿在这里留了个洞：要求覆盖「消费耗尽 → 密钥被停用」，却没说消费从哪来。
按仓库既有惯例解决 —— `tests/e2e/src/harness` 已经提供 `startOpenAiUpstream`
等桩上游，全部 e2e 都是**真网关 + 真 etcd + 本地桩上游**。只有 LLM 供应商是
桩（对着付费接口打不出确定性的流量），网关、指标、花费全是真的。不自创第二套。

**第一层 · 钱路（`tests/e2e/`，无浏览器）**

用既有 harness 起真网关，打真请求产生真花费，断言控制环确实扣了钱、确实停了密钥。

- [ ] **Step 1** 沿用 harness 起网关 + `startOpenAiUpstream`，seed 一把
      `user_id=u1` 的密钥与带定价的模型；readiness gate 按 `AGENTS.md`
      的规矩放在**最后 seed 的那把密钥**上
- [ ] **Step 2** 起门户，发放少量额度
- [ ] **Step 3** 打真请求直到超出额度
- [ ] **Step 4** 断言：账本余额转负、`resources.yaml` 里 u1 的密钥
      `disabled: true`、u2 的**没被碰**
- [ ] **Step 5** 补额后断言恢复

**第二层 · 人路（`web-portal/e2e/`，Playwright）**

对照 `web/e2e/fixture.ts`：拉起**真实** `aisix-portal` 进程与 `vite preview`，
随机空闲端口，`--host 127.0.0.1`。

- [ ] **Step 6** 注册 → 登录 → 余额为 0 → 管理员发放 → 余额可见
- [ ] **Step 7** 隔离用例：**两个用户各自登录，A 无论换什么参数都读不到 B
      的任何数字**
- [ ] **Step 8** 未绑定密钥时页面明确显示「未绑定」（H2 的可见性那一半）

- [ ] **Step 9：全绿**
- [ ] **Step 10：对每条新用例做 RED 校验**（改坏产品代码，确认用例会红；
      用**完整未过滤输出**核对 —— 编译错误会被 grep 掉，读到的 "ok" 是上一次
      的陈旧结果，这个坑本会话踩过两次）
- [ ] **Step 11：提交** `test(portal): end-to-end coverage`

---

## 自查

**Spec 覆盖：** spec §7 一期列了用户表、注册/登录/会话、门户进程、只读用量、
管理员发放、控制环——分别对应 Task 1/2-3/1/6/5/7，前端与 E2E 是交付所需。
二至四期（密钥自助、累计闸、支付）**不在本计划内**，符合 spec 的拆分。

**占位符扫描：** 无 TBD/TODO；每个代码步骤都有可运行的代码块或明确到可直接
落笔的规格。

**类型一致性：** `user_id` 全程为 `TEXT`/`&str`（uuid v4），与
`ApiKey.user_id: Option<String>` 对齐；金额全程 `i64` micro-USD 整数，
不出现浮点。

**已知不足（有意留下，不是遗漏）：** 一期结束时用户**还不能自助生成密钥**，
必须由管理员在控制台手工创建并填 `user_id`。因此一期交付的是"身份与钱的底座"
而非终端可用的产品；二期补上密钥自助后才形成完整闭环。

**审计后的修订（2026-08-26）：** 初稿有两处会**静默丢钱**的缺陷，已修：

1. 对账原本在累计 counter 上做差。花费指标每个网关副本各自一份，任一副本
   重启都会让 `sum` 下陷、被当成重置而重新对齐水位线，未入账的消费永久丢失
   且毫无信号。改为按时间窗查 `increase()`（逐序列处理重置后再求和），水位线
   改记时刻、且只在入账成功后前进。Task 1 的表与 Task 7 的全部测试随之重写。
2. `user_id` 由管理员手输，错一个字符就是永不扣款的免费用户。发放端改为只接受
   已存在的用户 id，门户增加「未绑定密钥」的显式状态，让静默失败变成可见状态。

另修：扣减必须允许余额为负（否则超出的那笔消费会被丢弃，又是白送）；
E2E 补上了「消费从哪来」的答案，沿用仓库既有 harness 的真网关 + 桩上游。
