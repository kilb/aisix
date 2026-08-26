//! `aisix-portal` —— 外部用户的自助门户。
//!
//! 与 `aisix-console` **分进程**：控制台是一个共享口令的全权管理端，不能同时
//! 服务陌生人。角色判断错一次就是全量泄漏；进程分开后，门户里根本不存在配置
//! 编辑的代码路径。
//!
//! 网关零改动：门户只读指标、只写自己的账本，以及在余额归零时把密钥置
//! `disabled`。它**绝不**出现在网关的请求路径上 —— 那会让网关的可用性被计费
//! 系统绑架。

mod auth;
mod store;

use axum::routing::post;
use axum::Router;
use store::Store;

/// 路由装配。抽成函数是为了测试能拿到同一个 app 而不必复制路由表 ——
/// 复制出来的路由表迟早跟生产的那份漂开。
pub fn router(store: Store) -> Router {
    Router::new()
        .route("/api/register", post(auth::register))
        .with_state(store)
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
    if let Err(e) = axum::serve(listener, router(store)).await {
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
        pub http: reqwest::Client,
    }

    impl TestApp {
        pub async fn start() -> Self {
            let store = Store::open_memory().await.unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = router(store.clone());
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Self {
                base: format!("http://{addr}"),
                store,
                http: reqwest::Client::new(),
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
