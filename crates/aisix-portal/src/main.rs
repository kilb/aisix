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
mod ledger;
mod store;

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
        .route("/admin/users", get(admin::list_users))
        .route("/admin/users/{id}/grant", post(admin::grant))
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

    let addr = std::env::var("PORTAL_ADDR").unwrap_or_else(|_| "127.0.0.1:8091".to_string());
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("监听 {addr} 失败: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("aisix-portal listening on {addr}");
    // 没配管理凭据时管理端整个关闭 —— 默认拒绝，而不是默认放行。
    let admin_token = std::env::var("PORTAL_ADMIN_TOKEN").ok();
    if admin_token.is_none() {
        eprintln!("PORTAL_ADMIN_TOKEN 未设置——管理端接口已关闭");
    }
    let state = AppState::with_admin_token(store, ARGON2_GATE_PERMITS, admin_token);
    if let Err(e) = axum::serve(listener, router(state)).await {
        eprintln!("serve 失败: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod testutil {
    use super::*;

    /// 起一个真监听、真 HTTP 的门户实例。
    ///
    /// 与 `crates/aisix-console` 的单测同一形状：不用 `oneshot`，因为那绕过了
    /// 真实的 HTTP 层（头部、状态码、序列化），而这些正是要断言的东西。
    pub struct TestApp {
        pub base: String,
        pub store: Store,
        pub state: AppState,
        pub http: reqwest::Client,
    }

    impl TestApp {
        pub async fn start() -> Self {
            Self::start_with_gate(ARGON2_GATE_PERMITS).await
        }

        /// `permits = 0` 让闸门永远拿不到许可，用来确定性地测 429，
        /// 而不必真去打并发（那种测试是 flaky 的）。
        pub const ADMIN_TOKEN: &str = "test-admin-token";

        pub async fn start_with_gate(permits: usize) -> Self {
            Self::build(permits, Some(Self::ADMIN_TOKEN.to_string())).await
        }

        /// 不配管理凭据的实例，用来断言「未配置时管理端整个关闭」。
        pub async fn start_without_admin_token() -> Self {
            Self::build(ARGON2_GATE_PERMITS, None).await
        }

        async fn build(permits: usize, admin_token: Option<String>) -> Self {
            let store = Store::open_memory().await.unwrap();
            let state = AppState::with_admin_token(store.clone(), permits, admin_token);
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
        let entries = ledger.entries(&id).await.unwrap();
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
