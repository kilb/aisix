//! `aisix-portal` —— 外部用户的自助门户。
//!
//! 与 `aisix-console` **分进程**：控制台是一个共享口令的全权管理端，不能同时
//! 服务陌生人。角色判断错一次就是全量泄漏；进程分开后，门户里根本不存在配置
//! 编辑的代码路径。
//!
//! 网关零改动：门户只读指标、只写自己的账本，以及在余额归零时把密钥置
//! `disabled`。它**绝不**出现在网关的请求路径上 —— 那会让网关的可用性被计费
//! 系统绑架。

mod admin;
mod auth;
mod client;
mod keys;
mod ledger;
mod logs;
mod metrics;
mod resources;
mod store;
mod sweeper;
mod topup;
mod usage;

use auth::AppState;
use axum::routing::{get, post};
use axum::Router;
use store::Store;

/// 未认证可达的 argon2 操作的并发上限。argon2 默认单次占 19 MiB，门户面向
/// 陌生人，不闸就是一条内存耗尽 DoS。
const ARGON2_GATE_PERMITS: usize = 4;

/// 路由装配。抽成函数是为了测试能拿到同一个 app 而不必复制路由表 ——
/// 复制出来的路由表迟早跟生产的那份漂开。
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/register", post(auth::register))
        .route("/api/login", post(auth::login))
        .route("/api/logout", post(auth::logout))
        .route("/api/session", get(auth::session))
        .route("/api/usage", get(usage::usage))
        .route("/api/logs", get(logs::logs))
        .route("/api/topups", get(topup::mine).post(topup::create))
        .route("/admin/topups", get(topup::pending))
        .route("/admin/topups/{id}/approve", post(topup::approve))
        .route("/admin/topups/{id}/reject", post(topup::reject))
        .route("/api/balance", get(usage::balance))
        .route("/api/keys", get(keys::list).post(keys::create))
        .route("/api/keys/{name}", axum::routing::delete(keys::revoke))
        .route(
            "/api/keys/{name}/quota",
            axum::routing::put(keys::set_quota),
        )
        .route("/admin/users", get(admin::list_users))
        .route("/admin/users/{id}/grant", post(admin::grant))
        .route("/admin/users/{id}/quota", post(admin::set_quota))
        .route("/admin/users/{id}/suspend", post(admin::suspend))
        .with_state(state)
}

#[tokio::main]
async fn main() {
    let db = std::env::var("PORTAL_DB").unwrap_or_else(|_| "sqlite:portal.db".to_string());
    let store = match Store::open(&db).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("打不开账本数据库 {db}: {e}");
            std::process::exit(1);
        }
    };

    // 没配管理凭据时管理端整个关闭 —— 默认拒绝，而不是默认放行。
    //
    // **空串等于没配。** `env::var(..).ok()` 会把 `PORTAL_ADMIN_TOKEN=` 变成
    // `Some("")`，而请求头 `Authorization: Bearer ` 去掉前缀后也是空串，两个空串
    // 常量时间比较为真 —— 于是「凭据配错」变成了「管理端对所有人敞开」，任何人
    // 都能给自己发额度。部署模板漏填就是这个形态。控制台那边对口令散列写的正是
    // `Ok(h) if !h.is_empty()`，这里跟上。
    let admin_token = std::env::var("PORTAL_ADMIN_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    if admin_token.is_none() {
        eprintln!("PORTAL_ADMIN_TOKEN 未设置或为空——管理端接口已关闭");
    }
    let prom_url =
        std::env::var("PROMETHEUS_URL").unwrap_or_else(|_| "http://127.0.0.1:9090".into());
    let resources_path =
        std::env::var("AISIX_RESOURCES").unwrap_or_else(|_| "/etc/aisix/resources.yaml".into());

    let state = AppState::build(
        store.clone(),
        ARGON2_GATE_PERMITS,
        admin_token,
        prom_url.clone(),
        resources_path,
    );

    let met = metrics::Metrics::new();
    // 指标口单独监听，不经 nginx —— 里面是运维数据，不该出现在租户能打到的
    // 地方。默认只在环回上。
    {
        let m = met.clone();
        let addr =
            std::env::var("PORTAL_METRICS_ADDR").unwrap_or_else(|_| "127.0.0.1:8092".to_string());
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/metrics",
                axum::routing::get(move || {
                    let m = m.clone();
                    async move {
                        (
                            [(
                                axum::http::header::CONTENT_TYPE,
                                "text/plain; version=0.0.4",
                            )],
                            m.render(),
                        )
                    }
                }),
            );
            match tokio::net::TcpListener::bind(&addr).await {
                Ok(l) => {
                    println!("aisix-portal metrics on {addr}");
                    let _ = axum::serve(l, app).await;
                }
                // 指标口起不来不该拖垮门户本身。
                Err(e) => eprintln!("指标口 {addr} 监听失败: {e}"),
            }
        });
    }

    // 对账环。周期 15 秒 —— 超支上界 ≈ 本周期内的消费 + 在途，而真正压住超支
    // 的是速率上限（设计文档 §3.2），不是这个数。
    //
    // 用 `state` 手里那个 Writer，不另建一个：铸密钥与停用密钥都写同一个文件，
    // 两条路径必须共用同一把串行锁，否则会互相覆盖。
    {
        // Arc：每一轮把它交给一个子任务（见下面的 panic 兜底）。
        let sw = std::sync::Arc::new(sweeper::Sweeper::new(
            store.clone(),
            sweeper::PromSource::new(prom_url),
            state.resources().clone(),
        ));
        let m = met.clone();
        tokio::spawn(async move {
            let mut tick =
                tokio::time::interval(std::time::Duration::from_secs(sweeper::tick_secs()));
            // **跳过错过的轮次，不要补齐。**
            //
            // `interval` 默认是 Burst：一轮跑超时之后，错过的几轮会背靠背立刻
            // 补上。对账是幂等的，补齐没有意义 —— 而它恰好在系统刚吃力过的那
            // 一刻，再压上一串满负荷的对账（每轮 N 个用户的 Prometheus 查询加
            // 一次写盘）。跳过就是「下一班车照常发」。
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                // **一轮里的 panic 不能杀掉整个循环。**
                //
                // 这个循环本身是 spawn 出去的：里面 panic，任务就没了，之后再也
                // 没有对账 —— 计费、额度下推、密钥启停全部停摆，而进程还好好活
                // 着、接口照常应答，没有任何东西会说出来。
                //
                // 每一轮再 spawn 一层：tokio 会把子任务的 panic 收成 `JoinError`
                // 交回来，循环因此活得下去。这比一个依赖换来的 `catch_unwind`
                // 更省事，行为也一样。
                let once = sw.clone();
                match tokio::spawn(async move { once.tick(chrono::Utc::now()).await }).await {
                    Ok(Ok(r)) => m.record_tick(&r),
                    Ok(Err(e)) => {
                        eprintln!("对账失败（水位线未前进，下一轮窗口会更长）: {e}");
                        m.record_tick_error();
                    }
                    Err(e) => {
                        eprintln!("对账这一轮没跑完（{e}），下一轮继续");
                        m.record_tick_error();
                    }
                }
            }
        });
    }

    let addr = std::env::var("PORTAL_ADDR").unwrap_or_else(|_| "127.0.0.1:8091".to_string());
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("监听 {addr} 失败: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("aisix-portal listening on {addr}");
    if let Err(e) = axum::serve(listener, router(state)).await {
        eprintln!("serve 失败: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod testutil {
    use super::*;
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    /// 起一个真监听、真 HTTP 的门户实例。
    ///
    /// 与 `crates/aisix-console` 的单测同一形状：不用 `oneshot`，因为那绕过了
    /// 真实的 HTTP 层（头部、状态码、序列化），而这些正是要断言的东西。
    /// 一个真的 Prometheus：真监听、真 HTTP，把收到的 `query` 记下来。
    ///
    /// 不用 mock 库 —— 与 `crates/aisix-console` 的单测同一形状。要断言的
    /// 正是「发出去的那条查询长什么样」，而这只有在真 HTTP 层才看得到。
    pub struct FakeProm {
        pub base: String,
        queries: Arc<Mutex<Vec<String>>>,
    }

    impl FakeProm {
        pub async fn start() -> Self {
            let queries: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let seen = queries.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = axum::Router::new().route(
                "/api/v1/query",
                axum::routing::get(move |q: axum::extract::RawQuery| {
                    let seen = seen.clone();
                    async move {
                        if let Some(raw) = q.0 {
                            seen.lock().unwrap().push(raw);
                        }
                        axum::Json(serde_json::json!({
                            "status": "success",
                            "data": {"resultType": "vector",
                                     "result": [{"metric": {}, "value": [0, "7"]}]}
                        }))
                    }
                }),
            );
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Self {
                base: format!("http://{addr}"),
                queries,
            }
        }

        pub fn seen(&self) -> Vec<String> {
            self.queries.lock().unwrap().clone()
        }
    }

    /// 写一份临时的 resources.yaml，返回路径。
    /// 配置文件放在**自己的目录**里。
    ///
    /// 直接扔进 /tmp 的话，「把写入路径变成不可写」这种失败注入就只能去 chmod
    /// /tmp —— 那会波及整台机器。写盘改成「同目录临时文件 + rename」之后，只把
    /// 文件设成只读也拦不住写入了（目录可写就能 rename 覆盖），所以必须能单独
    /// 控制那个目录。
    pub fn temp_resources(body: &str) -> String {
        let dir = std::env::temp_dir().join(format!("aisix-portal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("resources.yaml");
        std::fs::write(&p, body).unwrap();
        p.to_string_lossy().to_string()
    }

    /// 对账环需要一个消费来源，而这些用例不关心消费。返回 0 而不是 None ——
    /// None 是「读不到」，会让水位线停在原地，那是另一条分支。
    pub struct NoConsumption;

    impl crate::sweeper::ConsumptionSource for NoConsumption {
        async fn spend_in_window(
            &self,
            _user_id: &str,
            _from: chrono::DateTime<chrono::Utc>,
            _to: chrono::DateTime<chrono::Utc>,
        ) -> Option<u64> {
            Some(0)
        }

        async fn key_spend_in_window(
            &self,
            _api_key_id: &str,
            _from: chrono::DateTime<chrono::Utc>,
            _to: chrono::DateTime<chrono::Utc>,
        ) -> Option<u64> {
            Some(0)
        }
    }

    pub struct TestApp {
        pub base: String,
        pub store: Store,
        pub state: AppState,
        pub http: reqwest::Client,
        /// 本实例的 resources.yaml 路径。注册之后才知道 user_id，所以配置
        /// 必须能在注册后重写 —— 否则测试只能去拼凑 id，那是上一版失控的原因。
        resources_path: String,
    }

    impl TestApp {
        pub async fn start() -> Self {
            Self::start_with_gate(ARGON2_GATE_PERMITS).await
        }

        pub async fn start_with_gate(permits: usize) -> Self {
            Self::build(permits, Some(Self::ADMIN_TOKEN.to_string())).await
        }

        /// 不配管理凭据的实例，用来断言「未配置时管理端整个关闭」。
        pub async fn start_without_admin_token() -> Self {
            Self::build(ARGON2_GATE_PERMITS, None).await
        }

        /// `permits = 0` 让闸门永远拿不到许可，用来确定性地测 429，
        /// 而不必真去打并发（那种测试是 flaky 的）。
        pub const ADMIN_TOKEN: &str = "test-admin-token";

        /// 接上真 Prometheus 与一份配置文件的实例。
        pub async fn start_with_usage(prom: &FakeProm, resources_path: &str) -> Self {
            Self::build_full(
                ARGON2_GATE_PERMITS,
                Some(Self::ADMIN_TOKEN.to_string()),
                prom.base.clone(),
                resources_path.to_string(),
            )
            .await
        }

        async fn build(permits: usize, admin_token: Option<String>) -> Self {
            Self::build_full(permits, admin_token, String::new(), String::new()).await
        }

        async fn build_full(
            permits: usize,
            admin_token: Option<String>,
            prom_url: String,
            resources_path: String,
        ) -> Self {
            let resources_path_for_app = resources_path.clone();
            let store = Store::open_memory().await.unwrap();
            let state = AppState::build(
                store.clone(),
                permits,
                admin_token,
                prom_url,
                resources_path,
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = router(state.clone());
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Self {
                base: format!("http://{addr}"),
                store,
                state,
                resources_path: resources_path_for_app,
                // cookie_store 让登录后的会话自动带上，测的是真实浏览器行为。
                http: reqwest::Client::builder()
                    .cookie_store(true)
                    .build()
                    .unwrap(),
            }
        }

        pub async fn post(&self, path: &str, body: serde_json::Value) -> (u16, String) {
            let r = self
                .http
                .post(format!("{}{path}", self.base))
                .json(&body)
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }

        pub async fn get(&self, path: &str) -> (u16, String) {
            let r = self
                .http
                .get(format!("{}{path}", self.base))
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }

        /// 直接看 Set-Cookie 头，用于断言 cookie 的属性。
        pub async fn post_raw_headers(&self, path: &str, body: serde_json::Value) -> Vec<String> {
            let r = reqwest::Client::new()
                .post(format!("{}{path}", self.base))
                .json(&body)
                .send()
                .await
                .unwrap();
            r.headers()
                .get_all(reqwest::header::SET_COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok().map(str::to_string))
                .collect()
        }

        /// 带管理凭据发 POST。
        pub async fn post_admin(&self, path: &str, body: serde_json::Value) -> (u16, String) {
            let r = reqwest::Client::new()
                .post(format!("{}{path}", self.base))
                .bearer_auth(Self::ADMIN_TOKEN)
                .json(&body)
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }

        /// 带管理凭据发 GET。
        pub async fn get_admin(&self, path: &str) -> (u16, String) {
            let r = reqwest::Client::new()
                .get(format!("{}{path}", self.base))
                .bearer_auth(Self::ADMIN_TOKEN)
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }

        /// 重写本实例的 resources.yaml。
        pub fn set_resources(&self, body: &str) {
            std::fs::write(&self.resources_path, body).unwrap();
        }

        /// 把配置文件设成只读，用来测「读得到但写不进去」那条分支。
        pub fn make_resources_read_only(&self) {
            // 改**目录**的权限，不是文件的：写盘走「同目录临时文件 + rename」，
            // 目录可写就照样能覆盖过去。
            let dir = std::path::Path::new(&self.resources_path).parent().unwrap();
            let mut perm = std::fs::metadata(dir).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(true);
            std::fs::set_permissions(dir, perm).unwrap();
        }

        /// 读回本实例的 resources.yaml。
        pub fn resources(&self) -> String {
            std::fs::read_to_string(&self.resources_path).unwrap()
        }

        /// 跑一轮对账。用真 [`Sweeper`]，因为要测的正是「它写下的东西与接口
        /// 这一侧的改动能不能同时成立」—— 自己手写一份策略测的是别的东西。
        pub async fn tick(&self) {
            let sw = crate::sweeper::Sweeper::new(
                self.store.clone(),
                NoConsumption,
                crate::resources::Writer::new(self.resources_path.clone()),
            );
            sw.tick(chrono::Utc::now()).await.unwrap();
        }

        pub async fn put(&self, path: &str, body: serde_json::Value) -> (u16, String) {
            let r = self
                .http
                .put(format!("{}{path}", self.base))
                .json(&body)
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }

        pub async fn delete(&self, path: &str) -> (u16, String) {
            let r = self
                .http
                .delete(format!("{}{path}", self.base))
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }

        /// 读本实例的 resources.yaml。
        pub fn read_resources(&self) -> String {
            std::fs::read_to_string(&self.resources_path).unwrap_or_default()
        }

        pub async fn register(&self, email: &str, pw: &str) -> String {
            let (s, body) = self
                .post(
                    "/api/register",
                    serde_json::json!({"email": email, "password": pw}),
                )
                .await;
            assert_eq!(s, 201, "注册失败: {body}");
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["user_id"]
                .as_str()
                .unwrap()
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::TestApp;
    use serde_json::json;

    #[tokio::test]
    async fn 口令以_argon2_落库_绝不明文() {
        let app = TestApp::start().await;
        let (status, _) = app
            .post(
                "/api/register",
                json!({"email":"a@b.c","password":"correct horse battery"}),
            )
            .await;
        assert_eq!(status, 201);

        let row = app.store.user_by_email("a@b.c").await.unwrap().unwrap();
        // 两条都要断：不含明文，且确实是 argon2 而不是别的什么散列。
        assert!(!row.password_hash.contains("correct horse"));
        assert!(
            row.password_hash.starts_with("$argon2"),
            "落库的不是 argon2：{}",
            row.password_hash
        );
    }

    #[tokio::test]
    async fn 重复邮箱返回_409_而不是_500() {
        let app = TestApp::start().await;
        app.post(
            "/api/register",
            json!({"email":"a@b.c","password":"xxxxxxxxxxxx"}),
        )
        .await;
        let (status, _) = app
            .post(
                "/api/register",
                json!({"email":"a@b.c","password":"yyyyyyyyyyyy"}),
            )
            .await;
        assert_eq!(status, 409);
    }

    #[tokio::test]
    async fn 过短的口令被拒() {
        let app = TestApp::start().await;
        let (status, _) = app
            .post("/api/register", json!({"email":"a@b.c","password":"short"}))
            .await;
        assert_eq!(status, 400);
    }

    #[tokio::test]
    async fn 邮箱大小写与空白被归一_不能靠改大小写绕开唯一约束() {
        let app = TestApp::start().await;
        let (a, _) = app
            .post(
                "/api/register",
                json!({"email":"  User@B.C  ","password":"xxxxxxxxxxxx"}),
            )
            .await;
        assert_eq!(a, 201);
        // 同一个邮箱换个写法再来一次，必须仍被唯一约束挡住。
        let (b, _) = app
            .post(
                "/api/register",
                json!({"email":"user@b.c","password":"yyyyyyyyyyyy"}),
            )
            .await;
        assert_eq!(b, 409);
    }
}

#[cfg(test)]
mod login_tests {
    use super::testutil::TestApp;
    use serde_json::json;

    const PW: &str = "correct horse battery";

    #[tokio::test]
    async fn 口令错误与账号不存在的返回完全一致() {
        let app = TestApp::start().await;
        app.register("a@b.c", PW).await;

        let bad_pw = app
            .post(
                "/api/login",
                json!({"email":"a@b.c","password":"wrong wrong wrong"}),
            )
            .await;
        let no_user = app
            .post(
                "/api/login",
                json!({"email":"nobody@b.c","password":"wrong wrong wrong"}),
            )
            .await;

        // 两者必须无法区分，否则登录接口就是账号枚举器。
        assert_eq!(bad_pw.0, no_user.0);
        assert_eq!(bad_pw.1, no_user.1);
        assert_eq!(bad_pw.0, 401);
    }

    #[tokio::test]
    async fn 账号不存在时也跑一次_argon2_校验_抹平计时差() {
        let app = TestApp::start().await;
        let before = app.state.verification_count();
        app.post(
            "/api/login",
            json!({"email":"nobody@b.c","password":"whatever12345"}),
        )
        .await;
        // 返回体一致挡不住计时侧信道：不存在的账号如果直接短路返回，
        // 它会明显更快，攻击者据此就能枚举账号。所以这一次校验必须发生。
        //
        // 断言次数而不是断言时间 —— 计时测试必然 flaky，而这个性质是确定的。
        assert_eq!(app.state.verification_count(), before + 1);
    }

    #[tokio::test]
    async fn 被停用的账号返回与普通口令错一致_不泄漏账号状态() {
        let app = TestApp::start().await;
        let blocked = app.register("blocked@b.c", PW).await;
        app.register("live@b.c", PW).await;
        sqlx::query("UPDATE users SET disabled = 1 WHERE id = ?1")
            .bind(&blocked)
            .execute(app.store.pool())
            .await
            .unwrap();

        // 对照组必须是**另一个正常账号**配错口令。
        //
        // 第一版拿的是「同一个已停用账号 + 错口令」—— 两边都命中停用分支，
        // 于是无论产品怎么改都恒等，是一条空测试。RED 校验才把它揪出来：
        // 断言看着在比两件事，实际在比同一件事。
        let disabled_right_pw = app
            .post("/api/login", json!({"email":"blocked@b.c","password":PW}))
            .await;
        let live_wrong_pw = app
            .post(
                "/api/login",
                json!({"email":"live@b.c","password":"wrong wrong wrong"}),
            )
            .await;

        // 「这个账号被停用了」本身也是信息，不该从登录接口漏出去。
        assert_eq!(disabled_right_pw, live_wrong_pw);
        assert_eq!(disabled_right_pw.0, 401);
    }

    #[tokio::test]
    async fn 未登录时会话接口不返回任何用户信息() {
        let app = TestApp::start().await;
        let (status, body) = app.get("/api/session").await;
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["authed"], json!(false));
        // 连邮箱的形状都不该出现。
        assert!(!body.contains('@'), "未登录的会话响应里出现了 @：{body}");
    }

    #[tokio::test]
    async fn 登录后会话认得本人_登出后失效() {
        let app = TestApp::start().await;
        let id = app.register("a@b.c", PW).await;

        let (s, _) = app
            .post("/api/login", json!({"email":"a@b.c","password":PW}))
            .await;
        assert_eq!(s, 200);

        let (_, body) = app.get("/api/session").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["authed"], json!(true));
        assert_eq!(v["user_id"], json!(id));

        app.post("/api/logout", json!({})).await;
        let (_, after) = app.get("/api/session").await;
        let v2: serde_json::Value = serde_json::from_str(&after).unwrap();
        assert_eq!(v2["authed"], json!(false));
    }

    #[tokio::test]
    async fn 会话_cookie_带上_httponly_secure_与_samesite() {
        let app = TestApp::start().await;
        app.register("a@b.c", PW).await;
        let cookies = app
            .post_raw_headers("/api/login", json!({"email":"a@b.c","password":PW}))
            .await;
        let c = cookies.join(" ");
        // 三条缺一不可：HttpOnly 防脚本读取，SameSite=Strict 防别站借用，
        // Secure 因为门户只经 HTTPS 暴露。
        assert!(c.contains("HttpOnly"), "{c}");
        assert!(c.contains("Secure"), "{c}");
        assert!(c.contains("SameSite=Strict"), "{c}");
    }

    #[tokio::test]
    async fn 注册与登录共用同一个_argon2_并发闸() {
        let app = TestApp::start_with_gate(0).await;
        // argon2 默认单次占 19 MiB，两个端点都是未认证可达的。门户面向陌生人，
        // 任一端点没闸就是一条内存耗尽 DoS —— Task 2 的注册当时就漏了。
        let (reg, _) = app
            .post("/api/register", json!({"email":"a@b.c","password":PW}))
            .await;
        let (log, _) = app
            .post("/api/login", json!({"email":"a@b.c","password":PW}))
            .await;
        assert_eq!(reg, 429, "注册没有过闸");
        assert_eq!(log, 429, "登录没有过闸");
    }
}

#[cfg(test)]
mod admin_tests {
    use super::testutil::TestApp;
    use serde_json::json;

    const PW: &str = "correct horse battery";

    #[tokio::test]
    async fn 用户会话打不开管理端() {
        let app = TestApp::start().await;
        app.register("a@b.c", PW).await;
        app.post("/api/login", json!({"email":"a@b.c","password":PW}))
            .await;

        // 带着**合法的用户会话**去敲管理端。这条是裁决 3 的全部意义：
        // 两套凭据互不通用。若管理端认的是「同一套会话 + is_admin 标志」，
        // 判错一次就是全量泄漏。
        let (s, body) = app.get("/admin/users").await;
        assert_eq!(s, 401, "用户会话进了管理端: {body}");
    }

    #[tokio::test]
    async fn 没配管理凭据时管理端整个关闭_而不是放行() {
        let app = TestApp::start_without_admin_token().await;
        let (s, _) = app.get_admin("/admin/users").await;
        // 默认拒绝。配置缺失时放行是这类系统最常见的灾难形状。
        assert_eq!(s, 401);
    }

    #[tokio::test]
    async fn 发放额度会落成一条可审计的流水() {
        let app = TestApp::start().await;
        let id = app.register("a@b.c", PW).await;
        let (s, _) = app
            .post_admin(
                &format!("/admin/users/{id}/grant"),
                json!({"micro_usd": 5_000_000, "note": "首充赠送"}),
            )
            .await;
        assert_eq!(s, 200);

        let ledger = crate::ledger::Ledger::new(app.store.clone());
        let entries = ledger.entries(&id, 1_000).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].delta_micro_usd, 5_000_000);
        assert_eq!(entries[0].source, "admin_grant");
        // 谁发的、发了多少、凭什么 —— 记账操作必须留得下痕迹。
        assert_eq!(entries[0].note.as_deref(), Some("首充赠送"));
    }

    #[tokio::test]
    async fn 发放负数被拒_而不是变成扣款() {
        let app = TestApp::start().await;
        let id = app.register("a@b.c", PW).await;
        let (s, _) = app
            .post_admin(
                &format!("/admin/users/{id}/grant"),
                json!({"micro_usd": -1_000_000}),
            )
            .await;
        assert_eq!(s, 400);
        let ledger = crate::ledger::Ledger::new(app.store.clone());
        assert_eq!(ledger.balance(&id).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn 发放对象不存在时回_404_不留下孤立流水() {
        let app = TestApp::start().await;
        // 一期密钥由管理员手工创建并填 user_id。手输 uuid 错一个字符，网关照常
        // 放行、指标打错标签、门户查不到用量 —— 于是永不扣款，用户免费用而没人
        // 会发现。发放端只接受已存在的用户，把这条路堵掉。
        let (s, _) = app
            .post_admin(
                "/admin/users/not-a-real-user/grant",
                json!({"micro_usd": 1_000_000}),
            )
            .await;
        assert_eq!(s, 404);

        let ledger = crate::ledger::Ledger::new(app.store.clone());
        assert_eq!(
            ledger.balance("not-a-real-user").await.unwrap(),
            0,
            "给不存在的用户留下了流水"
        );
    }

    #[tokio::test]
    async fn 用户列表带上余额_供管理界面选择而非手输() {
        let app = TestApp::start().await;
        let id = app.register("a@b.c", PW).await;
        app.post_admin(
            &format!("/admin/users/{id}/grant"),
            json!({"micro_usd": 2_500_000}),
        )
        .await;

        let (s, body) = app.get_admin("/admin/users").await;
        assert_eq!(s, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let u = &v["users"][0];
        assert_eq!(u["user_id"], json!(id));
        assert_eq!(u["balance_micro_usd"], json!(2_500_000));
        // 散列绝不能出现在管理端响应里。
        assert!(!body.contains("$argon2"), "管理端漏出了口令散列");
    }
}

#[cfg(test)]
mod usage_tests {
    use super::testutil::{temp_resources, FakeProm, TestApp};
    use serde_json::json;

    const PW: &str = "correct horse battery";

    /// 一份含两个用户各自密钥的配置。
    fn doc(u1: &str, u2: &str) -> String {
        format!(
            "api_keys:\n\
             - display_name: a\n  key_hash: aa\n  user_id: {u1}\n\
             - display_name: b\n  key_hash: bb\n  user_id: {u2}\n"
        )
    }

    #[tokio::test]
    async fn 未登录读不到任何用量() {
        let prom = FakeProm::start().await;
        let app = TestApp::start_with_usage(&prom, &temp_resources("api_keys: []")).await;
        let (s, _) = app.get("/api/usage?range_hours=24").await;
        assert_eq!(s, 401);
        // 而且不该有任何查询被发出去 —— 未登录连查都不该查。
        assert!(
            prom.seen().is_empty(),
            "未登录也发了查询: {:?}",
            prom.seen()
        );
    }

    #[tokio::test]
    async fn 端点不接受调用方提供的查询() {
        let prom = FakeProm::start().await;
        let app = TestApp::start_with_usage(&prom, &temp_resources("api_keys: []")).await;
        let me = app.register("a@b.c", PW).await;
        app.post("/api/login", json!({"email":"a@b.c","password":PW}))
            .await;

        // 三种夹带都不该起作用。租户隔离是端点的形状 —— 它压根不读这些参数。
        for probe in [
            "/api/usage?query=sum(aisix_llm_spend_micro_usd_total)",
            "/api/usage?user_id=someone-else",
            "/api/usage?range_hours=24&user_id=someone-else",
        ] {
            let (s, body) = app.get(probe).await;
            assert_eq!(s, 200, "{probe}");
            assert!(!body.contains("someone-else"), "{probe} → {body}");
        }

        // 发出去的每一条查询都只带会话用户的 id。
        for q in prom.seen() {
            assert!(q.contains(&me), "查询里没有会话用户的 id: {q}");
            assert!(!q.contains("someone-else"), "夹带的 id 进了查询: {q}");
            assert!(
                !q.contains("by+%28user_id%29") && !q.contains("by (user_id)"),
                "出现了跨租户聚合: {q}"
            );
        }
    }

    #[tokio::test]
    async fn 两个用户各自读到自己的_id() {
        let prom = FakeProm::start().await;
        let app = TestApp::start_with_usage(&prom, &temp_resources("api_keys: []")).await;
        let a = app.register("a@b.c", PW).await;
        let b = app.register("b@b.c", PW).await;

        app.post("/api/login", json!({"email":"a@b.c","password":PW}))
            .await;
        app.get("/api/usage").await;
        let after_a = prom.seen().len();
        assert!(prom.seen().iter().all(|q| q.contains(&a)));

        // 换人登录（同一个 client，cookie 被覆盖）。
        app.post("/api/login", json!({"email":"b@b.c","password":PW}))
            .await;
        app.get("/api/usage").await;
        let for_b = &prom.seen()[after_a..];
        assert!(!for_b.is_empty());
        for q in for_b {
            assert!(q.contains(&b), "B 的查询里不是 B 的 id: {q}");
            assert!(!q.contains(&a), "B 的查询里出现了 A 的 id: {q}");
        }
    }

    #[tokio::test]
    async fn 一把密钥都没有时明确说出来并给出下一步() {
        let prom = FakeProm::start().await;
        // 配置里有两把密钥，但都不属于即将注册的这个人。
        let path = temp_resources(&doc("someone-1", "someone-2"));
        let app = TestApp::start_with_usage(&prom, &path).await;
        app.register("a@b.c", PW).await;
        app.post("/api/login", json!({"email":"a@b.c","password":PW}))
            .await;

        let (_, body) = app.get("/api/usage").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        // 「一把密钥都没有」时用量恒为 0，跟「还没开始用」在屏幕上没有区别。
        // 所以必须明确说出来，并且给出**当前**可执行的下一步。
        assert_eq!(v["linked_keys"], json!(0));
        assert!(body.contains("还没有密钥"), "{body}");
        assert!(body.contains("创建一把"), "{body}");
        // 这句话曾经是「请让管理员创建密钥」—— 用户能自助建之后，照做只会白等。
        assert!(!body.contains("让管理员创建密钥"), "{body}");
    }

    #[tokio::test]
    async fn 有密钥时不再报没有密钥_且计出停用数() {
        let prom = FakeProm::start().await;
        let app = TestApp::start_with_usage(&prom, &temp_resources("api_keys: []")).await;
        let me = app.register("a@b.c", PW).await;
        // 注册之后才知道 user_id，所以配置在这里才写得出来。
        app.set_resources(&format!(
            "api_keys:\n\
             - display_name: mine\n  key_hash: aa\n  user_id: {me}\n\
             - display_name: mine-off\n  key_hash: bb\n  user_id: {me}\n  disabled: true\n\
             - display_name: other\n  key_hash: cc\n  user_id: someone-else\n"
        ));
        app.post("/api/login", json!({"email":"a@b.c","password":PW}))
            .await;

        let (_, body) = app.get("/api/usage").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["linked_keys"], json!(2));
        assert_eq!(v["disabled_keys"], json!(1));
        assert!(!body.contains("未绑定"), "{body}");
    }

    #[tokio::test]
    async fn 配置读不到时不假装有绑定() {
        let prom = FakeProm::start().await;
        // 指向一个不存在的路径。
        let app = TestApp::start_with_usage(&prom, "/nonexistent/aisix/resources.yaml").await;
        app.register("a@b.c", PW).await;
        app.post("/api/login", json!({"email":"a@b.c","password":PW}))
            .await;

        let (_, body) = app.get("/api/usage").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        // 保守方向：读不到就当成「没有密钥」，促使人去看，而不是让人以为一切正常。
        assert_eq!(v["linked_keys"], json!(0));
        assert!(body.contains("还没有密钥"));
    }
}

#[cfg(test)]
mod balance_tests {
    use super::testutil::{temp_resources, FakeProm, TestApp};
    use serde_json::json;

    const PW: &str = "correct horse battery";

    #[tokio::test]
    async fn 未登录读不到余额() {
        let prom = FakeProm::start().await;
        let app = TestApp::start_with_usage(&prom, &temp_resources("api_keys: []")).await;
        let (s, body) = app.get("/api/balance").await;
        assert_eq!(s, 401);
        assert!(!body.contains("balance_micro_usd"));
    }

    #[tokio::test]
    async fn 只读到自己的余额与流水() {
        let prom = FakeProm::start().await;
        let app = TestApp::start_with_usage(&prom, &temp_resources("api_keys: []")).await;
        let a = app.register("a@b.c", PW).await;
        let b = app.register("b@b.c", PW).await;
        app.post_admin(
            &format!("/admin/users/{a}/grant"),
            json!({"micro_usd": 5_000_000, "note": "给 A 的"}),
        )
        .await;
        app.post_admin(
            &format!("/admin/users/{b}/grant"),
            json!({"micro_usd": 9_000_000, "note": "给 B 的"}),
        )
        .await;

        app.post("/api/login", json!({"email":"a@b.c","password":PW}))
            .await;
        let (_, body) = app.get("/api/balance").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["balance_micro_usd"], json!(5_000_000));
        // B 的流水绝不能出现在 A 的响应里。
        assert!(!body.contains("给 B 的"), "串账了: {body}");
        assert_eq!(v["entries"].as_array().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod keys_tests {
    use super::testutil::{temp_resources, FakeProm, TestApp};
    use serde_json::json;

    const PW: &str = "correct horse battery";
    /// 网关**真能加载**的一份配置。残缺文档会让写前校验那道闸走「本来就坏、
    /// 照写」的降级路径，于是它在这批用例里全程没被执行过。
    const SEED: &str = r#"_format_version: "1"
provider_keys:
  - display_name: stub
    provider: openai
    api_key: sk-stub
models:
  - display_name: keep-me
    provider: openai
    provider_key: stub
    model_name: keep-me
api_keys:
  - display_name: ops
    key_hash: opsopsopsopsopsopsopsopsopsopsops
    allowed_models: ["*"]
"#;

    async fn app() -> (FakeProm, TestApp) {
        let prom = FakeProm::start().await;
        let app = TestApp::start_with_usage(&prom, &temp_resources(SEED)).await;
        (prom, app)
    }

    async fn signed_in(app: &TestApp, mail: &str) -> String {
        let id = app.register(mail, PW).await;
        app.post("/api/login", json!({"email": mail, "password": PW}))
            .await;
        id
    }

    #[tokio::test]
    async fn 未登录不能铸密钥() {
        let (_p, app) = app().await;
        let (s, _) = app.post("/api/keys", json!({})).await;
        assert_eq!(s, 401);
    }

    #[tokio::test]
    async fn 明文只在铸出来那一次给出_落盘只有散列() {
        let (_p, app) = app().await;
        signed_in(&app, "a@b.c").await;
        let (s, body) = app.post("/api/keys", json!({"label": "我的第一把"})).await;
        assert_eq!(s, 201, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let plaintext = v["plaintext"].as_str().unwrap().to_string();
        assert!(plaintext.starts_with("sk-aisix-"));

        let yaml = app.read_resources();
        // 明文绝不能落盘。
        assert!(!yaml.contains(&plaintext), "明文进了配置文件");
        assert!(
            yaml.contains(&crate::keys::sha256_hex(&plaintext)),
            "散列没落盘"
        );

        // 列表接口也拿不到明文，散列还得是遮蔽的。
        let (_, list) = app.get("/api/keys").await;
        assert!(!list.contains(&plaintext));
        assert!(list.contains('…'), "散列没遮: {list}");
    }

    #[tokio::test]
    async fn 零余额时新密钥生下来就是停用的() {
        let (_p, app) = app().await;
        signed_in(&app, "a@b.c").await;
        let (_, body) = app.post("/api/keys", json!({})).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        // 若建成可用的，它会在网关眼里活到对账环下一轮才被关掉 —— 那一段是
        // 白送的推理，而且每建一把密钥就送一次。
        assert_eq!(v["disabled"], json!(true));
        assert!(v["note"].as_str().unwrap().contains("停用"));
        assert!(app.read_resources().contains("disabled: true"));
    }

    #[tokio::test]
    async fn 有余额时新密钥直接可用() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        app.post_admin(
            &format!("/admin/users/{id}/grant"),
            json!({"micro_usd": 5_000_000}),
        )
        .await;
        let (_, body) = app.post("/api/keys", json!({})).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["disabled"], json!(false));
        assert!(v["note"].is_null());
    }

    #[tokio::test]
    async fn 可以铸任意多把_且都绑到本人() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        for i in 0..4 {
            let (s, _) = app
                .post("/api/keys", json!({"label": format!("k{i}")}))
                .await;
            assert_eq!(s, 201);
        }
        let (_, list) = app.get("/api/keys").await;
        let v: serde_json::Value = serde_json::from_str(&list).unwrap();
        assert_eq!(v["keys"].as_array().unwrap().len(), 4);
        // 每一把都带本人的 user_id —— 少了它，网关的指标打不上标签，
        // 门户查不到用量，对账环也找不到这把密钥去停用。
        assert_eq!(app.read_resources().matches(&id).count(), 4);
    }

    #[tokio::test]
    async fn 铸密钥不会抹掉配置里门户不认识的东西() {
        let (_p, app) = app().await;
        signed_in(&app, "a@b.c").await;
        app.post("/api/keys", json!({})).await;
        let yaml = app.read_resources();
        assert!(yaml.contains("keep-me"), "models 段被抹掉了:\n{yaml}");
        assert!(yaml.contains("ops"), "运维自己的密钥被抹掉了:\n{yaml}");
    }

    #[tokio::test]
    async fn 列表只给本人的密钥() {
        let (_p, app) = app().await;
        signed_in(&app, "a@b.c").await;
        app.post("/api/keys", json!({"label": "A的"})).await;

        signed_in(&app, "b@b.c").await; // 同一个 client，cookie 被覆盖
        app.post("/api/keys", json!({"label": "B的"})).await;

        let (_, list) = app.get("/api/keys").await;
        let v: serde_json::Value = serde_json::from_str(&list).unwrap();
        assert_eq!(
            v["keys"].as_array().unwrap().len(),
            1,
            "看到了别人的密钥: {list}"
        );
        assert!(list.contains("B的"));
        assert!(!list.contains("A的"));
    }

    #[tokio::test]
    async fn 只能吊销自己的密钥() {
        let (_p, app) = app().await;
        signed_in(&app, "a@b.c").await;
        let (_, body) = app.post("/api/keys", json!({"label": "A的"})).await;
        let a_name = serde_json::from_str::<serde_json::Value>(&body).unwrap()["name"]
            .as_str()
            .unwrap()
            .to_string();

        signed_in(&app, "b@b.c").await;
        // B 拿着 A 的密钥名去删。少了 user_id 那一半判断，任何登录用户都能
        // 凭名字删掉别人的密钥 —— 包括运维那些没有 user_id 的。
        let (s, _) = app.delete(&format!("/api/keys/{a_name}")).await;
        assert_eq!(s, 404, "B 删掉了 A 的密钥");
        assert!(app.read_resources().contains("A的"), "A 的密钥被删了");
    }

    #[tokio::test]
    async fn 吊销自己的密钥会真的从配置里消失() {
        let (_p, app) = app().await;
        signed_in(&app, "a@b.c").await;
        let (_, body) = app.post("/api/keys", json!({"label": "待删"})).await;
        let name = serde_json::from_str::<serde_json::Value>(&body).unwrap()["name"]
            .as_str()
            .unwrap()
            .to_string();

        let (s, _) = app.delete(&format!("/api/keys/{name}")).await;
        assert_eq!(s, 200);
        assert!(!app.read_resources().contains("待删"));
        // 运维的密钥还在。
        assert!(app.read_resources().contains("ops"));
    }
}

#[cfg(test)]
mod topup_tests {
    use super::testutil::{temp_resources, FakeProm, TestApp};
    use serde_json::json;

    const PW: &str = "correct horse battery";

    async fn app() -> (FakeProm, TestApp) {
        let prom = FakeProm::start().await;
        let app = TestApp::start_with_usage(&prom, &temp_resources("api_keys: []")).await;
        (prom, app)
    }

    async fn signed_in(app: &TestApp, mail: &str) -> String {
        let id = app.register(mail, PW).await;
        app.post("/api/login", json!({"email": mail, "password": PW}))
            .await;
        id
    }

    async fn balance(app: &TestApp, uid: &str) -> i64 {
        crate::ledger::Ledger::new(app.store.clone())
            .balance(uid)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn 发起充值单不会立刻入账() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        let (s, _) = app
            .post("/api/topups", json!({"micro_usd": 5_000_000}))
            .await;
        assert_eq!(s, 201);
        // 钱还没到，余额就不能动 —— 这一条是「线下」两个字的全部含义。
        assert_eq!(balance(&app, &id).await, 0);
    }

    #[tokio::test]
    async fn 负数与超大金额被拒() {
        let (_p, app) = app().await;
        signed_in(&app, "a@b.c").await;
        for bad in [-1_000_000i64, 0, 10_000_000_001] {
            let (s, _) = app.post("/api/topups", json!({"micro_usd": bad})).await;
            assert_eq!(s, 400, "金额 {bad} 没被拒");
        }
    }

    #[tokio::test]
    async fn 批准后入账_且流水来源是充值() {
        let (_p, app) = app().await;
        let uid = signed_in(&app, "a@b.c").await;
        app.post(
            "/api/topups",
            json!({"micro_usd": 5_000_000, "note": "转账截图见邮件"}),
        )
        .await;

        let (_, list) = app.get_admin("/admin/topups").await;
        let v: serde_json::Value = serde_json::from_str(&list).unwrap();
        let tid = v["topups"][0]["id"].as_i64().unwrap();
        assert_eq!(v["topups"][0]["email"], json!("a@b.c"));

        let (s, _) = app
            .post_admin(
                &format!("/admin/topups/{tid}/approve"),
                json!({"note": "已核对"}),
            )
            .await;
        assert_eq!(s, 200);
        assert_eq!(balance(&app, &uid).await, 5_000_000);

        let entries = crate::ledger::Ledger::new(app.store.clone())
            .entries(&uid, 1_000)
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, "topup");
    }

    #[tokio::test]
    async fn 重复批准不会重复入账() {
        let (_p, app) = app().await;
        let uid = signed_in(&app, "a@b.c").await;
        app.post("/api/topups", json!({"micro_usd": 5_000_000}))
            .await;
        let (_, list) = app.get_admin("/admin/topups").await;
        let tid = serde_json::from_str::<serde_json::Value>(&list).unwrap()["topups"][0]["id"]
            .as_i64()
            .unwrap();

        let (a, _) = app
            .post_admin(&format!("/admin/topups/{tid}/approve"), json!({}))
            .await;
        let (b, _) = app
            .post_admin(&format!("/admin/topups/{tid}/approve"), json!({}))
            .await;
        assert_eq!(a, 200);
        // 第二次必须是冲突而不是「成功」—— 假装成功会让管理员以为自己刚入了账。
        assert_eq!(b, 409);
        assert_eq!(balance(&app, &uid).await, 5_000_000, "同一笔充值入账了两次");
    }

    #[tokio::test]
    async fn 驳回不入账_且不能再被批准() {
        let (_p, app) = app().await;
        let uid = signed_in(&app, "a@b.c").await;
        app.post("/api/topups", json!({"micro_usd": 5_000_000}))
            .await;
        let (_, list) = app.get_admin("/admin/topups").await;
        let tid = serde_json::from_str::<serde_json::Value>(&list).unwrap()["topups"][0]["id"]
            .as_i64()
            .unwrap();

        app.post_admin(
            &format!("/admin/topups/{tid}/reject"),
            json!({"note": "没收到款"}),
        )
        .await;
        assert_eq!(balance(&app, &uid).await, 0);
        let (s, _) = app
            .post_admin(&format!("/admin/topups/{tid}/approve"), json!({}))
            .await;
        assert_eq!(s, 409);
        assert_eq!(balance(&app, &uid).await, 0);
    }

    #[tokio::test]
    async fn 用户只看得到自己的充值单() {
        let (_p, app) = app().await;
        signed_in(&app, "a@b.c").await;
        app.post(
            "/api/topups",
            json!({"micro_usd": 1_000_000, "note": "A的单子"}),
        )
        .await;
        signed_in(&app, "b@b.c").await;
        app.post(
            "/api/topups",
            json!({"micro_usd": 2_000_000, "note": "B的单子"}),
        )
        .await;

        let (_, mine) = app.get("/api/topups").await;
        assert!(mine.contains("B的单子"));
        assert!(!mine.contains("A的单子"), "看到了别人的充值单: {mine}");
    }

    #[tokio::test]
    async fn 用户会话打不开充值单的管理端() {
        let (_p, app) = app().await;
        signed_in(&app, "a@b.c").await;
        let (s, _) = app.get("/admin/topups").await;
        assert_eq!(s, 401);
    }
}

#[cfg(test)]
mod topup_race_tests {
    use super::testutil::{temp_resources, FakeProm, TestApp};
    use serde_json::json;

    /// 两个管理员同时点确认。
    ///
    /// 串行的重复批准由 SELECT 里的 `status = 'pending'` 就挡住了；影响行数那
    /// 道防线**只在真并发下才起作用** —— 两个事务都查到 pending，然后靠 UPDATE
    /// 的影响行数决出唯一的赢家。没有这条测试，那段代码等于没被验证过。
    #[tokio::test]
    async fn 并发批准同一笔只入账一次() {
        let prom = FakeProm::start().await;
        let app = TestApp::start_with_usage(&prom, &temp_resources("api_keys: []")).await;
        let uid = app.register("a@b.c", "correct horse battery").await;
        app.post(
            "/api/login",
            json!({"email":"a@b.c","password":"correct horse battery"}),
        )
        .await;
        app.post("/api/topups", json!({"micro_usd": 5_000_000}))
            .await;

        let (_, list) = app.get_admin("/admin/topups").await;
        let tid = serde_json::from_str::<serde_json::Value>(&list).unwrap()["topups"][0]["id"]
            .as_i64()
            .unwrap();

        let base = app.base.clone();
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..6 {
            let url = format!("{base}/admin/topups/{tid}/approve");
            set.spawn(async move {
                reqwest::Client::new()
                    .post(url)
                    .bearer_auth(TestApp::ADMIN_TOKEN)
                    .json(&json!({}))
                    .send()
                    .await
                    .map(|r| r.status().as_u16())
                    .unwrap_or(0)
            });
        }
        let mut ok = 0;
        let mut conflict = 0;
        while let Some(r) = set.join_next().await {
            match r.unwrap() {
                200 => ok += 1,
                409 => conflict += 1,
                other => panic!("意外状态码 {other}"),
            }
        }
        assert_eq!(ok, 1, "有不止一个调用者认为自己入了账");
        assert_eq!(conflict, 5);

        let bal = crate::ledger::Ledger::new(app.store.clone())
            .balance(&uid)
            .await
            .unwrap();
        assert_eq!(bal, 5_000_000, "同一笔充值入账了多次");
    }
}

#[cfg(test)]
mod quota_tests {
    use super::testutil::{temp_resources, FakeProm, TestApp};
    use serde_json::json;

    const PW: &str = "correct horse battery";
    /// 网关**真能加载**的一份配置。残缺文档会让写前校验那道闸走「本来就坏、
    /// 照写」的降级路径，于是它在这批用例里全程没被执行过。
    const SEED: &str = r#"_format_version: "1"
provider_keys:
  - display_name: stub
    provider: openai
    api_key: sk-stub
models:
  - display_name: keep-me
    provider: openai
    provider_key: stub
    model_name: keep-me
api_keys:
  - display_name: ops
    key_hash: opsopsopsopsopsopsopsopsopsopsops
    allowed_models: ["*"]
"#;

    async fn app() -> (FakeProm, TestApp) {
        let prom = FakeProm::start().await;
        let app = TestApp::start_with_usage(&prom, &temp_resources(SEED)).await;
        (prom, app)
    }

    async fn signed_in(app: &TestApp, mail: &str) -> String {
        let id = app.register(mail, PW).await;
        app.post("/api/login", json!({"email": mail, "password": PW}))
            .await;
        id
    }

    async fn mint(app: &TestApp, label: &str) -> String {
        let (_, body) = app.post("/api/keys", json!({"label": label})).await;
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["name"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn 管理员设定额度是绝对值_不是追加() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;

        app.post_admin(
            &format!("/admin/users/{id}/quota"),
            json!({"micro_usd": 5_000_000}),
        )
        .await;
        app.post_admin(
            &format!("/admin/users/{id}/quota"),
            json!({"micro_usd": 8_000_000}),
        )
        .await;

        let l = crate::ledger::Ledger::new(app.store.clone());
        // 设两次 5 和 8，总额是 8 而不是 13 —— 这是「设定」跟「发放」的全部区别。
        assert_eq!(l.total_granted(&id).await.unwrap(), 8_000_000);
    }

    #[tokio::test]
    async fn 调低额度会记一条负数流水_总额跟着落() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        app.post_admin(
            &format!("/admin/users/{id}/quota"),
            json!({"micro_usd": 9_000_000}),
        )
        .await;
        app.post_admin(
            &format!("/admin/users/{id}/quota"),
            json!({"micro_usd": 2_000_000}),
        )
        .await;

        let l = crate::ledger::Ledger::new(app.store.clone());
        // 按正负筛总额的写法在这里会漏掉负数那条，总额只涨不落 —— 网关那边的
        // 闸也就降不下来。所以总额必须按**来源**算。
        assert_eq!(l.total_granted(&id).await.unwrap(), 2_000_000);
        let entries = l.entries(&id, 1_000).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].delta_micro_usd, -7_000_000);
        assert_eq!(entries[1].source, "admin_set");
    }

    #[tokio::test]
    async fn 荒谬的大额度被拒_而不是把闸开到无穷() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        for path in ["quota", "grant"] {
            let (s, _) = app
                .post_admin(
                    &format!("/admin/users/{id}/{path}"),
                    json!({"micro_usd": 900_000_000_000_000i64}),
                )
                .await;
            assert_eq!(s, 400, "{path} 接受了一个多打了几个零的数");
        }
        let l = crate::ledger::Ledger::new(app.store.clone());
        assert_eq!(l.total_granted(&id).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn 负数额度被拒() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        let (s, _) = app
            .post_admin(
                &format!("/admin/users/{id}/quota"),
                json!({"micro_usd": -1}),
            )
            .await;
        assert_eq!(s, 400);
    }

    #[tokio::test]
    async fn 密钥额度之和不得超过用户总额() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        app.post_admin(
            &format!("/admin/users/{id}/quota"),
            json!({"micro_usd": 10_000_000}),
        )
        .await;
        let a = mint(&app, "一").await;
        let b = mint(&app, "二").await;

        assert_eq!(
            app.put(
                &format!("/api/keys/{a}/quota"),
                json!({"micro_usd": 6_000_000})
            )
            .await
            .0,
            200
        );
        // 6 + 5 > 10，必须被拒。
        let (s, body) = app
            .put(
                &format!("/api/keys/{b}/quota"),
                json!({"micro_usd": 5_000_000}),
            )
            .await;
        assert_eq!(s, 409, "{body}");
        assert!(body.contains("available_micro_usd"), "{body}");
        // 6 + 4 = 10，刚好，放行。
        assert_eq!(
            app.put(
                &format!("/api/keys/{b}/quota"),
                json!({"micro_usd": 4_000_000})
            )
            .await
            .0,
            200
        );
    }

    #[tokio::test]
    async fn 调低某把密钥的额度不会被自己的旧值挡住() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        app.post_admin(
            &format!("/admin/users/{id}/quota"),
            json!({"micro_usd": 10_000_000}),
        )
        .await;
        let a = mint(&app, "一").await;
        app.put(
            &format!("/api/keys/{a}/quota"),
            json!({"micro_usd": 10_000_000}),
        )
        .await;

        // 把它从 10 调到 3。校验若拿「当前总和 + 新值」比，会算成 13 > 10 而误拒。
        let (s, body) = app
            .put(
                &format!("/api/keys/{a}/quota"),
                json!({"micro_usd": 3_000_000}),
            )
            .await;
        assert_eq!(s, 200, "调低自己的额度被自己的旧值挡住了: {body}");
    }

    #[tokio::test]
    async fn 只能给自己的密钥设额度() {
        let (_p, app) = app().await;
        let a_id = signed_in(&app, "a@b.c").await;
        app.post_admin(
            &format!("/admin/users/{a_id}/quota"),
            json!({"micro_usd": 10_000_000}),
        )
        .await;
        let a_key = mint(&app, "A的").await;

        let b_id = signed_in(&app, "b@b.c").await;
        app.post_admin(
            &format!("/admin/users/{b_id}/quota"),
            json!({"micro_usd": 10_000_000}),
        )
        .await;
        let (s, _) = app
            .put(
                &format!("/api/keys/{a_key}/quota"),
                json!({"micro_usd": 1_000_000}),
            )
            .await;
        assert_eq!(s, 404, "B 给 A 的密钥设了额度");
    }

    #[tokio::test]
    async fn 额度设为零等于不单独设限() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        app.post_admin(
            &format!("/admin/users/{id}/quota"),
            json!({"micro_usd": 10_000_000}),
        )
        .await;
        let a = mint(&app, "一").await;
        app.put(
            &format!("/api/keys/{a}/quota"),
            json!({"micro_usd": 6_000_000}),
        )
        .await;
        app.put(&format!("/api/keys/{a}/quota"), json!({"micro_usd": 0}))
            .await;

        // 清零之后那份额度回到可分配的池子里。
        assert_eq!(app.store.allocated_to_keys(&id).await.unwrap(), 0);
        let (_, list) = app.get("/api/keys").await;
        let v: serde_json::Value = serde_json::from_str(&list).unwrap();
        assert_eq!(v["keys"][0]["quota_micro_usd"], json!(0));
    }

    #[tokio::test]
    async fn 吊销带额度的密钥能成_且策略与密钥同批消失() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        app.post_admin(
            &format!("/admin/users/{id}/quota"),
            json!({"micro_usd": 10_000_000}),
        )
        .await;
        let a = mint(&app, "一").await;
        app.put(
            &format!("/api/keys/{a}/quota"),
            json!({"micro_usd": 6_000_000}),
        )
        .await;
        // 先让对账环把策略写进配置。
        app.tick().await;
        let doc = app.resources();
        assert!(doc.contains("portal-key-"), "策略没被下推: {doc}");

        // 吊销必须成功。策略若留到下一轮才撤，这份文档里就有一条指着已不存在
        // 密钥的策略 —— 写前校验会拒掉整次写入，吊销直接失败。
        let (s, body) = app.delete(&format!("/api/keys/{a}")).await;
        assert_eq!(s, 200, "吊销带额度的密钥失败了: {body}");
        let after = app.resources();
        assert!(!after.contains("portal-key-"), "策略被留下了: {after}");
        assert!(!after.contains(&a), "密钥还在: {after}");
    }

    #[tokio::test]
    async fn 吊销密钥会一并清掉它的额度() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        app.post_admin(
            &format!("/admin/users/{id}/quota"),
            json!({"micro_usd": 10_000_000}),
        )
        .await;
        let a = mint(&app, "一").await;
        app.put(
            &format!("/api/keys/{a}/quota"),
            json!({"micro_usd": 6_000_000}),
        )
        .await;

        app.delete(&format!("/api/keys/{a}")).await;
        // 留着的话下一轮对账还会为一把不存在的密钥下推策略。
        assert_eq!(app.store.allocated_to_keys(&id).await.unwrap(), 0);
    }
}

#[cfg(test)]
mod review_probes {
    use super::testutil::{temp_resources, FakeProm, TestApp};
    use serde_json::json;

    const PW: &str = "correct horse battery";
    const SEED: &str = r#"_format_version: "1"
provider_keys:
  - display_name: stub
    provider: openai
    api_key: sk-stub
models:
  - display_name: keep-me
    provider: openai
    provider_key: stub
    model_name: keep-me
api_keys:
  - display_name: ops
    key_hash: opsopsopsopsopsopsopsopsopsopsops
    allowed_models: ["*"]
"#;

    async fn app() -> (FakeProm, TestApp) {
        let prom = FakeProm::start().await;
        let app = TestApp::start_with_usage(&prom, &temp_resources(SEED)).await;
        (prom, app)
    }

    async fn signed_in(app: &TestApp, mail: &str) -> String {
        let id = app.register(mail, PW).await;
        app.post("/api/login", json!({"email": mail, "password": PW}))
            .await;
        id
    }

    async fn mint(app: &TestApp, label: &str) -> String {
        let (_, body) = app.post("/api/keys", json!({"label": label})).await;
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["name"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// 探针 1：超大额度不能绕过「之和 ≤ 总额」。
    #[tokio::test]
    async fn 巨大的额度值不能靠整数溢出绕过上限() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        app.post_admin(
            &format!("/admin/users/{id}/quota"),
            json!({"micro_usd": 10_000_000}),
        )
        .await;
        let a = mint(&app, "一").await;
        let b = mint(&app, "二").await;
        app.put(
            &format!("/api/keys/{a}/quota"),
            json!({"micro_usd": 6_000_000}),
        )
        .await;

        let (s, body) = app
            .put(
                &format!("/api/keys/{b}/quota"),
                json!({"micro_usd": i64::MAX}),
            )
            .await;
        assert_ne!(s, 200, "i64::MAX 被接受了: {body}");
        assert_eq!(
            app.store.allocated_to_keys(&id).await.unwrap(),
            6_000_000,
            "已分配额被写坏了"
        );
    }

    /// 探针 2：密钥被别处删掉后，它占的额度要还回可分配池。
    #[tokio::test]
    async fn 密钥从配置里消失后它占的额度要还回来() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        app.post_admin(
            &format!("/admin/users/{id}/quota"),
            json!({"micro_usd": 10_000_000}),
        )
        .await;
        let a = mint(&app, "一").await;
        let b = mint(&app, "二").await;
        app.put(
            &format!("/api/keys/{a}/quota"),
            json!({"micro_usd": 8_000_000}),
        )
        .await;

        // 运维从控制台把「一」删了 —— 门户的库里那条额度记录还在。
        let doc = app.resources();
        let kept: String = doc.lines().collect::<Vec<_>>().join("\n");
        let filtered = strip_key(&kept, &a);
        app.set_resources(&filtered);
        app.tick().await;

        // 那 8 块必须回到池子里，否则用户看着自己有 10 块却一分也分不出去。
        let (s, body) = app
            .put(
                &format!("/api/keys/{b}/quota"),
                json!({"micro_usd": 9_000_000}),
            )
            .await;
        assert_eq!(s, 200, "已删密钥占着的额度没还回来: {body}");
    }

    /// 并发给不同密钥设额度，之和仍不得越过总额。
    #[tokio::test]
    async fn 并发设置各把密钥的额度_之和不越过总额() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        app.post_admin(
            &format!("/admin/users/{id}/quota"),
            json!({"micro_usd": 10_000_000}),
        )
        .await;
        // 五把密钥，每把都想要 4 块 —— 总额只有 10 块，最多只能有两把成功。
        let mut names = Vec::new();
        for i in 0..5 {
            names.push(mint(&app, &format!("k{i}")).await);
        }

        let mut set = tokio::task::JoinSet::new();
        for n in names {
            let base = app.base.clone();
            // 克隆出来的 reqwest::Client 共用同一个 cookie 存储，会话跟着走。
            let http = app.http.clone();
            set.spawn(async move {
                http.put(format!("{base}/api/keys/{n}/quota"))
                    .json(&json!({"micro_usd": 4_000_000}))
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            });
        }
        let mut ok = 0;
        while let Some(r) = set.join_next().await {
            if r.unwrap() == 200 {
                ok += 1;
            }
        }

        // 校验在事务外时，五个请求会各自读到「已分配 0」而全部通过。
        let allocated = app.store.allocated_to_keys(&id).await.unwrap();
        assert!(
            allocated <= 10_000_000,
            "各把额度之和 {allocated} 越过了总额 10000000（成功 {ok} 个）"
        );
    }

    /// 并发把总额度设成不同的值，最终必须落在其中一个上。
    #[tokio::test]
    async fn 并发设定总额度_结果是其中一个目标而不是第三个数() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        app.post_admin(
            &format!("/admin/users/{id}/quota"),
            json!({"micro_usd": 5_000_000}),
        )
        .await;

        // 只发两个请求时读写基本不交错，测试无论有没有事务都会绿 —— 那是虚假
        // 信心。要把并发压到足以交错。
        let mut set = tokio::task::JoinSet::new();
        for i in 0..12 {
            let target: i64 = if i % 2 == 0 { 8_000_000 } else { 2_000_000 };
            let base = app.base.clone();
            let http = app.http.clone();
            let path = format!("/admin/users/{id}/quota");
            set.spawn(async move {
                http.post(format!("{base}{path}"))
                    .header("authorization", format!("Bearer {}", TestApp::ADMIN_TOKEN))
                    .json(&json!({"micro_usd": target}))
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            });
        }
        while let Some(r) = set.join_next().await {
            r.unwrap();
        }

        // 读改写不在一个事务里时，多个请求读到同一份旧值、各写一条按旧值算出的
        // 差额，最终总额落在一个谁也没要求过的数上，而每个请求都报了成功。
        let g = crate::ledger::Ledger::new(app.store.clone())
            .total_granted(&id)
            .await
            .unwrap();
        assert!(
            g == 8_000_000 || g == 2_000_000,
            "总额停在了 {g}，既不是 8000000 也不是 2000000"
        );
    }

    /// 流水多起来之后，余额接口不能把全部条目一次吐出来。
    #[tokio::test]
    async fn 余额接口的流水条数有上限_且余额本身仍算全部() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        let l = crate::ledger::Ledger::new(app.store.clone());
        // 对账环给有消费的用户每轮写一条，15 秒一轮就是一天约 5760 条。这里造
        // 300 条已经越过展示上限。
        for _ in 0..300 {
            l.credit(&id, 1_000, crate::ledger::Source::AdminGrant, None)
                .await
                .unwrap();
        }

        let (_, body) = app.get("/api/balance").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let n = v["entries"].as_array().unwrap().len();
        assert!(n <= 200, "一次吐了 {n} 条流水");
        assert_eq!(
            v["entries_truncated"],
            serde_json::json!(true),
            "截断了却没说"
        );
        // 余额是 SUM 出来的，不受展示上限影响 —— 截断的是给人看的那几条。
        assert_eq!(v["balance_micro_usd"], serde_json::json!(300_000));
    }

    #[tokio::test]
    async fn 账号被停用后_旧会话立刻失效() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        assert_eq!(app.get("/api/keys").await.0, 200);

        sqlx::query("UPDATE users SET disabled = 1 WHERE id = ?1")
            .bind(&id)
            .execute(app.store.pool())
            .await
            .unwrap();

        // 只在登录处看这个字段的话，刚被停用的人还能拿着旧会话继续铸密钥、设
        // 额度、提充值单，最长一个会话周期。
        assert_eq!(app.get("/api/keys").await.0, 401, "停用后旧会话仍然有效");
        assert_eq!(
            app.post("/api/keys", json!({"label": "还能建吗"})).await.0,
            401
        );
    }

    #[tokio::test]
    async fn 过长的密钥名称被拒() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        app.post_admin(
            &format!("/admin/users/{id}/quota"),
            json!({"micro_usd": 1_000_000}),
        )
        .await;
        // 名称原样进网关配置，也进下推的策略名。不限长的话一个请求就能把配置
        // 撑起来，而网关每次重载都要整份读一遍。
        let (s, _) = app
            .post("/api/keys", json!({"label": "长".repeat(500)}))
            .await;
        assert_eq!(s, 400, "过长的名称被接受了");
        assert!(!app.resources().contains(&"长".repeat(500)));
    }

    #[tokio::test]
    async fn 未处理的充值申请有个数上限() {
        let (_p, app) = app().await;
        signed_in(&app, "a@b.c").await;
        let mut accepted = 0;
        for _ in 0..10 {
            if app
                .post("/api/topups", json!({"micro_usd": 1_000_000}))
                .await
                .0
                == 201
            {
                accepted += 1;
            }
        }
        // 不设上限的话，一个登录用户能一直提，把管理员的待办列表刷爆，而管理员
        // 只能一笔笔驳回。
        assert!(accepted <= 5, "接受了 {accepted} 笔未处理申请");
        assert!(accepted >= 1, "一笔都没接受，那是另一个 bug");
    }

    #[tokio::test]
    async fn 过期会话既不生效_也不会一直堆着() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        // 塞一堆已经过期的条目。清理原本发生在每个请求的写锁里；改成只读之后
        // 必须仍然有地方把它们收走，否则这个表只增不减。
        for i in 0..50 {
            app.state.put_session(&format!("stale-{i}"), &id, 1).await;
        }
        assert!(app.state.session_count().await >= 50);

        // 拿一个不存在的令牌打一次 —— 未命中那条路径顺手清理。
        let r = app
            .http
            .get(format!("{}/api/keys", app.base))
            .header("cookie", "aisix_portal=nope")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401);
        // 只剩真正在用的那一条。
        assert_eq!(app.state.session_count().await, 1, "过期条目没被清掉");

        // 而且过期的令牌本身绝不能生效。
        let r = app
            .http
            .get(format!("{}/api/keys", app.base))
            .header("cookie", "aisix_portal=stale-0")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401, "过期会话仍然放行");
    }

    #[tokio::test]
    async fn 聚合查询与逐用户算出来的数一致() {
        let (_p, app) = app().await;
        let l = crate::ledger::Ledger::new(app.store.clone());
        let mut ids = Vec::new();
        for i in 0..4 {
            let id = signed_in(&app, &format!("u{i}@b.c")).await;
            ids.push(id.clone());
            // 各种来源都来一笔，让「算总额」与「算余额」的差别真的体现出来。
            app.post_admin(
                &format!("/admin/users/{id}/quota"),
                json!({"micro_usd": 10_000_000 + i * 1_000_000}),
            )
            .await;
            app.post_admin(
                &format!("/admin/users/{id}/grant"),
                json!({"micro_usd": 500_000}),
            )
            .await;
            let mut c = app.store.pool().acquire().await.unwrap();
            crate::ledger::insert_entry(
                &mut *c,
                &id,
                -(300_000 + i * 1_000),
                crate::ledger::Source::Consumption,
                None,
            )
            .await
            .unwrap();
            if i % 2 == 0 {
                let k = mint(&app, "k").await;
                app.put(
                    &format!("/api/keys/{k}/quota"),
                    json!({"micro_usd": 2_000_000}),
                )
                .await;
            }
        }
        // 还有一个一笔流水都没有的用户 —— 聚合查询里压根没有他这一行，
        // 调用方必须按 0 处理而不是漏掉他。
        let quiet = signed_in(&app, "quiet@b.c").await;
        ids.push(quiet);

        // 再塞一条**没被归类**的来源。两份分类逻辑（单用户查询与聚合查询）若
        // 各自漂移，这里就会露出来 —— 症状本来是「列表里的总额跟详情不一样」。
        let mut c = app.store.pool().acquire().await.unwrap();
        sqlx::query(
            "INSERT INTO ledger (user_id, delta_micro_usd, source, note, created_at)
             VALUES (?1, -7000, 'refund_reversal', NULL, '2026-08-28T00:00:00Z')",
        )
        .bind(&ids[0])
        .execute(&mut *c)
        .await
        .unwrap();
        drop(c);

        let sums = app.store.all_balances().await.unwrap();
        let allocs = app.store.all_allocated().await.unwrap();
        for id in &ids {
            let (b, g) = sums
                .iter()
                .find(|(u, _, _)| u == id)
                .map(|(_, b, g)| (*b, *g))
                .unwrap_or((0, 0));
            let a = allocs
                .iter()
                .find(|(u, _)| u == id)
                .map(|(_, a)| *a)
                .unwrap_or(0);
            assert_eq!(b, l.balance(id).await.unwrap(), "余额对不上: {id}");
            assert_eq!(g, l.total_granted(id).await.unwrap(), "总额对不上: {id}");
            assert_eq!(
                a,
                app.store.allocated_to_keys(id).await.unwrap(),
                "已分配对不上: {id}"
            );
        }

        // 接口输出也要能对上，包括那个没有流水的用户。
        let (_, body) = app.get_admin("/admin/users").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let users = v["users"].as_array().unwrap();
        assert_eq!(users.len(), ids.len());
        for u in users {
            let id = u["user_id"].as_str().unwrap();
            assert_eq!(
                u["balance_micro_usd"].as_i64().unwrap(),
                l.balance(id).await.unwrap()
            );
            assert_eq!(
                u["granted_micro_usd"].as_i64().unwrap(),
                l.total_granted(id).await.unwrap()
            );
        }
    }

    /// 对账环错过的轮次必须跳过，不能背靠背补齐。
    ///
    /// 钉源码而不是去测时序：要测出 Burst 与 Skip 的差别，得让一轮真的超时，
    /// 那种测试要么很慢要么很脆。这里要守的是一个明确的选择。
    #[test]
    fn 对账环跳过错过的轮次而不是补齐() {
        let src = include_str!("main.rs");
        let production = src
            .split_once("\n#[cfg(test)]")
            .map(|(before, _)| before)
            .unwrap_or(src);
        assert!(
            production.contains("MissedTickBehavior::Skip"),
            "对账环用的是默认的 Burst —— 一轮超时之后会背靠背补齐，正好压在系统刚吃力过的时候",
        );
    }

    #[tokio::test]
    async fn 连续失败若干次后进入冷却_成功一次即清零() {
        let (_p, app) = app().await;
        signed_in(&app, "a@b.c").await;

        // 前几次是普通的 401。
        for i in 0..5 {
            let (s, _) = app
                .post(
                    "/api/login",
                    json!({"email": "a@b.c", "password": "错的口令啊啊"}),
                )
                .await;
            assert_eq!(s, 401, "第 {i} 次应当是 401");
        }
        // 之后进冷却。**连正确口令也进不去** —— 冷却是按邮箱的，不看这次对不对，
        // 否则爆破只要最后一次猜对就穿了。
        let (s, _) = app
            .post(
                "/api/login",
                json!({"email": "a@b.c", "password": "错的口令啊啊"}),
            )
            .await;
        assert_eq!(s, 429, "连续失败之后没有进冷却");
        let (s, _) = app
            .post("/api/login", json!({"email": "a@b.c", "password": PW}))
            .await;
        assert_eq!(s, 429);

        // 冷却只对这个邮箱，别人不受影响 —— 否则拿别人的邮箱刷失败就能把整个
        // 门户的登录锁死。
        signed_in(&app, "b@b.c").await;

        // 换个没失败过的邮箱能正常登录，说明计数是按邮箱分开的。
        let (s, _) = app
            .post("/api/login", json!({"email": "b@b.c", "password": PW}))
            .await;
        assert_eq!(s, 200);
    }

    #[tokio::test]
    async fn 失败没到阈值时_成功登录会把计数清零() {
        let (_p, app) = app().await;
        signed_in(&app, "a@b.c").await;
        // 输错四次（没到阈值），成功一次，再输错四次 —— 若不清零，这里已经是
        // 第八次失败，会被误锁。偶尔输错的正常用户就是这个形态。
        for _ in 0..4 {
            app.post(
                "/api/login",
                json!({"email": "a@b.c", "password": "错的口令啊啊"}),
            )
            .await;
        }
        assert_eq!(
            app.post("/api/login", json!({"email": "a@b.c", "password": PW}))
                .await
                .0,
            200
        );
        for i in 0..4 {
            let (s, _) = app
                .post(
                    "/api/login",
                    json!({"email": "a@b.c", "password": "错的口令啊啊"}),
                )
                .await;
            assert_eq!(s, 401, "成功一次之后计数没清零（第 {i} 次就被锁了）");
        }
    }

    #[tokio::test]
    async fn 铸密钥有频率上限_但不限总数() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        app.post_admin(
            &format!("/admin/users/{id}/quota"),
            json!({"micro_usd": 10_000_000}),
        )
        .await;

        let mut ok = 0;
        let mut throttled = 0;
        for _ in 0..15 {
            match app.post("/api/keys", json!({"label": "k"})).await.0 {
                201 => ok += 1,
                429 => throttled += 1,
                other => panic!("意外状态 {other}"),
            }
        }
        // 每铸一把都要重写整份网关配置并发 SIGHUP，所以频率要有上限……
        assert!(throttled > 0, "循环铸密钥没有被限速");
        // ……但用户要的是「任意多把」，所以窗口内该放行的都要放行。
        assert_eq!(ok, 10, "窗口内放行的把数不对: {ok}");
    }

    /// 探针 3：配置写入失败时，库里的额度不能已经被改掉。
    #[tokio::test]
    async fn 写盘失败时不能留下改了一半的状态() {
        let (_p, app) = app().await;
        let id = signed_in(&app, "a@b.c").await;
        app.post_admin(
            &format!("/admin/users/{id}/quota"),
            json!({"micro_usd": 10_000_000}),
        )
        .await;
        let a = mint(&app, "一").await;
        app.put(
            &format!("/api/keys/{a}/quota"),
            json!({"micro_usd": 6_000_000}),
        )
        .await;

        // 让写盘失败：文件只读。读还能读，所以改动会算出来，只有落盘那一步失败。
        app.make_resources_read_only();
        let (s, _) = app.delete(&format!("/api/keys/{a}")).await;
        assert_ne!(s, 200, "写盘失败却报成功");

        // 密钥还在配置里（写没成），所以它的额度也必须还在。否则下一轮对账会
        // 把它的策略撤掉，这把密钥就静默变成「不单独设限」。
        assert_eq!(
            app.store.allocated_to_keys(&id).await.unwrap(),
            6_000_000,
            "写盘失败了，库里的额度却已经被删掉"
        );
    }

    /// 从 YAML 文本里删掉某把密钥那一段（够用的粗暴做法，只在测试里）。
    fn strip_key(doc: &str, short_name: &str) -> String {
        let mut out = Vec::new();
        let mut skipping = false;
        for line in doc.lines() {
            if line.starts_with("- display_name:") || line.starts_with("  - display_name:") {
                skipping = line.contains(short_name);
            }
            if !skipping {
                out.push(line);
            }
        }
        out.join("\n") + "\n"
    }
}
