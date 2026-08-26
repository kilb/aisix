//! 数据库访问。**唯一**碰 SQL 的地方。
//!
//! 一期用 SQLite。选 sqlx 而非 rusqlite 是因为它异步、与 axum/tokio 一致；
//! 换 Postgres 只需改 feature 与连接串。

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::str::FromStr;
use std::time::Duration;

/// 一条用户记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: String,
    pub email: String,
    pub password_hash: String,
    pub display_name: Option<String>,
    pub disabled: bool,
}

#[derive(Debug)]
pub enum StoreError {
    /// 邮箱已存在。注册处据此回 409，而不是 500 —— 唯一约束冲突是可预期的
    /// 用户错误，不是服务端故障。
    EmailTaken,
    Db(sqlx::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmailTaken => write!(f, "邮箱已被注册"),
            Self::Db(e) => write!(f, "数据库错误: {e}"),
        }
    }
}
impl std::error::Error for StoreError {}

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        // 唯一约束冲突要跟其它数据库错误分开，否则注册重复邮箱会变成 500。
        if let sqlx::Error::Database(ref db) = e {
            if db.message().contains("UNIQUE") && db.message().contains("users.email") {
                return Self::EmailTaken;
            }
        }
        Self::Db(e)
    }
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// 打开（或创建）一个库文件并应用 migrations。
    pub async fn open(path: &str) -> Result<Self, StoreError> {
        let opts = SqliteConnectOptions::from_str(path)
            .map_err(StoreError::Db)?
            .create_if_missing(true)
            // 并发写入撞 SQLITE_BUSY 是常态，不设就是间歇失败。
            .busy_timeout(Duration::from_secs(5));
        Self::from_options(opts, 5).await
    }

    /// 测试用的内存库。每个 `Store` 一个，互不可见。
    ///
    /// 实测过（sqlx 0.8）：`sqlite::memory:` 下**同一个池的多条连接共享同一个
    /// 库，不同池之间隔离**——正是这里要的两件事，不必自己拼 `cache=shared`
    /// 或给库起唯一名字。
    ///
    /// 记下这次实测是因为我先按「每条池连接各拿一个私有库」的旧经验写了一版
    /// 绕路代码，跑探针才发现 sqlx 已经处理了。
    pub async fn open_memory() -> Result<Self, StoreError> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(StoreError::Db)?
            .busy_timeout(Duration::from_secs(5));
        Self::from_options(opts, 5).await
    }

    async fn from_options(opts: SqliteConnectOptions, max_conns: u32) -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(max_conns)
            .connect_with(opts)
            .await
            .map_err(StoreError::Db)?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| StoreError::Db(sqlx::Error::Migrate(Box::new(e))))?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn insert_user(
        &self,
        id: &str,
        email: &str,
        password_hash: &str,
        display_name: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO users (id, email, password_hash, display_name, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(id)
        .bind(email)
        .bind(password_hash)
        .bind(display_name)
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn user_by_email(&self, email: &str) -> Result<Option<User>, StoreError> {
        let row: Option<(String, String, String, Option<String>, i64)> = sqlx::query_as(
            "SELECT id, email, password_hash, display_name, disabled
             FROM users WHERE email = ?1",
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;
        Ok(
            row.map(|(id, email, password_hash, display_name, disabled)| User {
                id,
                email,
                password_hash,
                display_name,
                disabled: disabled != 0,
            }),
        )
    }

    /// 全部用户，按注册时间。管理端据此提供**选择**而不是手输。
    pub async fn all_users(&self) -> Result<Vec<User>, StoreError> {
        let rows: Vec<(String, String, String, Option<String>, i64)> = sqlx::query_as(
            "SELECT id, email, password_hash, display_name, disabled
             FROM users ORDER BY created_at, id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, email, password_hash, display_name, disabled)| User {
                id,
                email,
                password_hash,
                display_name,
                disabled: disabled != 0,
            })
            .collect())
    }

    pub async fn user_by_id(&self, id: &str) -> Result<Option<User>, StoreError> {
        let row: Option<(String, String, String, Option<String>, i64)> = sqlx::query_as(
            "SELECT id, email, password_hash, display_name, disabled
             FROM users WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(
            row.map(|(id, email, password_hash, display_name, disabled)| User {
                id,
                email,
                password_hash,
                display_name,
                disabled: disabled != 0,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 建表后可以插入并读回一个用户() {
        let store = Store::open_memory().await.unwrap();
        store
            .insert_user("u1", "a@b.c", "hash", None)
            .await
            .unwrap();
        let u = store.user_by_email("a@b.c").await.unwrap().unwrap();
        assert_eq!(u.id, "u1");
    }

    #[tokio::test]
    async fn 同一邮箱不能注册两次() {
        let store = Store::open_memory().await.unwrap();
        store.insert_user("u1", "a@b.c", "h", None).await.unwrap();
        // 唯一约束必须由数据库拒绝，而不是靠调用方先查后插。
        let e = store
            .insert_user("u2", "a@b.c", "h", None)
            .await
            .unwrap_err();
        // 而且必须被识别为「邮箱已占用」，否则注册接口会把它渲染成 500。
        assert!(matches!(e, StoreError::EmailTaken), "得到的是 {e:?}");
    }

    #[tokio::test]
    async fn 内存库的两条池连接看到同一个库() {
        // Task 4 的并发测试依赖「同池的多条连接看到同一个库」。这条守卫盯着
        // 它：换成按连接隔离的配置就会红。
        //
        // 必须**同时持有**两条连接。第一版甩了 8 个并发读，结果池只建了一条
        // 连接就都服务完，「多条连接」从未发生 —— 那是一条空测试，RED 校验
        // 才把它揪出来。
        use sqlx::Acquire;
        let store = Store::open_memory().await.unwrap();

        let mut a = store.pool().acquire().await.unwrap();
        let mut b = store.pool().acquire().await.unwrap();

        sqlx::query(
            "INSERT INTO users (id, email, password_hash, created_at)
                     VALUES ('u1','shared@b.c','h','now')",
        )
        .execute(a.acquire().await.unwrap())
        .await
        .unwrap();

        let found: Option<(String,)> =
            sqlx::query_as("SELECT id FROM users WHERE email = 'shared@b.c'")
                .fetch_optional(b.acquire().await.unwrap())
                .await
                .expect("另一条池连接连表都读不到 —— cache=shared 没生效");
        assert_eq!(found.map(|r| r.0).as_deref(), Some("u1"));
    }
}
