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
  会有漂移（spec §3.1）；超支上界 = 轮询周期内消费 + 在途（spec §3.2）。

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
| `crates/aisix-portal/migrations/0001_init.sql` | users / ledger / sessions 表 |
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

-- 消费对账的水位线。记录每个用户已计入流水的累计消费额，
-- 下一轮只把「新读到的累计值 − 水位线」入账，避免重复扣。
CREATE TABLE consumption_mark (
  user_id             TEXT PRIMARY KEY REFERENCES users(id),
  counted_micro_usd   INTEGER NOT NULL,
  updated_at          TEXT NOT NULL
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
async fn 发放负数被拒_而不是变成扣款() {
    let app = test_app().await;
    let id = register(&app, "a@b.c", "correct horse battery").await;
    let r = post_admin(&app, &format!("/admin/users/{id}/grant"),
        json!({"micro_usd": -1_000_000})).await;
    assert_eq!(r.status, 400);
}
```

- [ ] **Step 2：跑测试确认失败**
- [ ] **Step 3：实现**
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
- [ ] **Step 4：跑测试确认通过**
- [ ] **Step 5：提交** `feat(portal): tenant-scoped usage queries`

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
async fn 只把新增的消费入账_不重复扣() {
    let s = test_sweeper().await;
    s.ledger.credit("u1", 5_000_000, "admin_grant", None).await.unwrap();

    s.prom.set_cumulative("u1", 1_000_000);
    s.tick().await.unwrap();
    assert_eq!(s.ledger.balance("u1").await.unwrap(), 4_000_000);

    // 累计值没变，第二轮不该再扣一次——水位线的全部意义在此。
    s.tick().await.unwrap();
    assert_eq!(s.ledger.balance("u1").await.unwrap(), 4_000_000);

    s.prom.set_cumulative("u1", 1_250_000);
    s.tick().await.unwrap();
    assert_eq!(s.ledger.balance("u1").await.unwrap(), 3_750_000);
}

#[tokio::test]
async fn 累计值回退时不入账_并且报出来() {
    let s = test_sweeper().await;
    s.ledger.credit("u1", 5_000_000, "admin_grant", None).await.unwrap();
    s.prom.set_cumulative("u1", 1_000_000);
    s.tick().await.unwrap();

    // 网关重启会让 counter 归零。差值为负时**绝不能**倒贴钱给用户。
    s.prom.set_cumulative("u1", 200_000);
    s.tick().await.unwrap();
    assert_eq!(s.ledger.balance("u1").await.unwrap(), 4_000_000);
    assert_eq!(s.counter_resets(), 1);
}

#[tokio::test]
async fn 余额归零会把该用户的密钥置_disabled() {
    let s = test_sweeper_with_config().await;   // 配置里有一把 user_id=u1 的密钥
    s.ledger.credit("u1", 1_000_000, "admin_grant", None).await.unwrap();
    s.prom.set_cumulative("u1", 1_200_000);
    s.tick().await.unwrap();

    assert!(s.ledger.balance("u1").await.unwrap() <= 0);
    let doc = s.read_resources();
    assert_eq!(doc.key_with_user("u1").disabled, true);
    // 别人的密钥不许被碰。
    assert_eq!(doc.key_with_user("u2").disabled, false);
}

#[tokio::test]
async fn 补上余额后密钥被重新启用() {
    let s = test_sweeper_with_config().await;
    s.ledger.credit("u1", 1_000_000, "admin_grant", None).await.unwrap();
    s.prom.set_cumulative("u1", 1_200_000);
    s.tick().await.unwrap();
    assert_eq!(s.read_resources().key_with_user("u1").disabled, true);

    s.ledger.credit("u1", 5_000_000, "admin_grant", None).await.unwrap();
    s.tick().await.unwrap();
    assert_eq!(s.read_resources().key_with_user("u1").disabled, false);
}
```

- [ ] **Step 2：跑测试确认失败**
- [ ] **Step 3：实现**。写 `resources.yaml` 用与控制台相同的内容散列乐观并发，
      撞版本重读重试（裁决 2）；轮询周期默认 15 秒（spec §3.2）；
      counter 回退只记 `counter_resets` 并把水位线降到新值，不入账
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

### Task 9：E2E

**Files:**
- Create: `web-portal/e2e/fixture.ts`、`web-portal/e2e/portal.spec.ts`

对照 `web/e2e/fixture.ts`：拉起**真实** `aisix-portal` 进程与 `vite preview`，
随机空闲端口，`--host 127.0.0.1`。

- [ ] **Step 1：夹具拉起真后端**
- [ ] **Step 2：写用例**

必须覆盖的真实路径：

```
注册 → 登录 → 余额为 0 → 管理员发放 → 余额可见 →
用量页只显示自己的数 → 消费耗尽 → 密钥被停用 → 补额 → 恢复
```

以及一条隔离用例：**两个用户各自登录，A 无论如何都读不到 B 的任何数字。**

- [ ] **Step 3：全绿**
- [ ] **Step 4：对每条新用例做 RED 校验**（改坏产品代码，确认用例会红；
      用**完整未过滤输出**核对，不要 grep 掉编译错误）
- [ ] **Step 5：提交** `test(portal): end-to-end coverage`

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
