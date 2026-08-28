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
    /// 金额算不下去了（相减会溢出 i64）。调用方据此回 400 —— 这是输入荒谬，
    /// 不是服务端故障。
    OutOfRange,
    Db(sqlx::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmailTaken => write!(f, "邮箱已被注册"),
            Self::OutOfRange => write!(f, "金额超出可表示范围"),
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
            .busy_timeout(Duration::from_secs(5))
            // **WAL，不是默认的 delete。** 实测 `from_str` 这条路给出的是
            // `journal_mode=delete`：写事务持排他锁，期间**所有读都被挡住**。
            // 对账环每轮都在写（扣款、推额度、开关密钥），而门户的每个请求都要
            // 读（会话、余额、密钥列表），于是那几百毫秒里请求全排队，撞到
            // `busy_timeout` 就是 500。WAL 下读不被写挡住。
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
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

    /// 跑一段**先读后写**的事务，用 `BEGIN IMMEDIATE`。
    ///
    /// 默认的 `BEGIN`（deferred）在并发下会死锁式失败：两个事务都先读、再都想
    /// 升级成写，其中一个当场拿到 `SQLITE_BUSY` —— 而 `busy_timeout` 救不了升级
    /// 冲突（它只在事务尚未持有读锁时才有机会等待）。表现是随机的 500，且只在
    /// 真并发下出现，所以串行测试永远看不到，实测靠一条并发用例才炸出来。
    ///
    /// `IMMEDIATE` 一开始就拿写锁，于是第二个事务是**等待**而不是失败。
    ///
    /// 单条 INSERT 不需要这个（一条语句本身原子）；先读后写的才需要。
    pub async fn immediate_tx<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        F: for<'c> FnOnce(
            &'c mut sqlx::SqliteConnection,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<T, StoreError>> + Send + 'c>,
        >,
    {
        let mut conn = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
        match f(&mut conn).await {
            Ok(v) => {
                sqlx::query("COMMIT").execute(&mut *conn).await?;
                Ok(v)
            }
            Err(e) => {
                // 回滚失败也不掩盖原始错误 —— 那才是要报出去的东西。
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                Err(e)
            }
        }
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

    /// 某个用户名下各把密钥的额度。
    pub async fn key_quotas(&self, user_id: &str) -> Result<Vec<(String, i64)>, StoreError> {
        let mut c = self.pool.acquire().await?;
        key_quotas_on(&mut *c, user_id).await
    }

    /// 全部用户的密钥额度，供对账环一次性下推。
    pub async fn all_key_quotas(&self) -> Result<Vec<(String, String, i64)>, StoreError> {
        let rows: Vec<(String, String, i64)> =
            sqlx::query_as("SELECT user_id, key_name, micro_usd FROM key_quotas")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }

    /// 已分配到各把密钥上的额度之和。
    pub async fn allocated_to_keys(&self, user_id: &str) -> Result<i64, StoreError> {
        let (sum,): (i64,) =
            sqlx::query_as("SELECT COALESCE(SUM(micro_usd), 0) FROM key_quotas WHERE user_id = ?1")
                .bind(user_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(sum)
    }

    pub async fn set_key_quota(
        &self,
        user_id: &str,
        key_name: &str,
        micro_usd: i64,
    ) -> Result<(), StoreError> {
        let mut c = self.pool.acquire().await?;
        set_key_quota_on(&mut *c, user_id, key_name, micro_usd).await?;
        Ok(())
    }

    /// 密钥被吊销时一并清掉它的额度 —— 留着的话下一轮对账还会为它下推策略，
    /// 而那把密钥已经不存在了。
    pub async fn drop_key_quota(&self, user_id: &str, key_name: &str) -> Result<(), StoreError> {
        let mut c = self.pool.acquire().await?;
        drop_key_quota_on(&mut *c, user_id, key_name).await
    }

    /// 已计入流水的截止时刻。`None` = 还没对过账。
    pub async fn counted_through(
        &self,
        user_id: &str,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>, StoreError> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT counted_through FROM consumption_mark WHERE user_id = ?1")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(t,)| {
            chrono::DateTime::parse_from_rfc3339(&t)
                .ok()
                .map(|d| d.with_timezone(&chrono::Utc))
        }))
    }

    pub async fn set_counted_through(
        &self,
        user_id: &str,
        t: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO consumption_mark (user_id, counted_through, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(user_id) DO UPDATE SET counted_through = ?2, updated_at = ?3",
        )
        .bind(user_id)
        .bind(t.to_rfc3339())
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
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

// ── 可挂在任意 executor 上的语句 ─────────────────────────────────────────
//
// 抽出来是为了让「读额度 → 校验 → 写额度」能跑在**同一个** BEGIN IMMEDIATE
// 事务里。各自在事务外单独跑的话，两个并发请求会各自读到同一份旧值、双双通过
// 校验，于是各把密钥的额度之和越过用户总额 —— 用户要的那条不变量就破了。
// 语句只此一份，事务内外共用，避免出现两个真相。

pub(crate) async fn key_quotas_on<'e, E>(
    exec: E,
    user_id: &str,
) -> Result<Vec<(String, i64)>, StoreError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT key_name, micro_usd FROM key_quotas WHERE user_id = ?1 ORDER BY key_name",
    )
    .bind(user_id)
    .fetch_all(exec)
    .await?;
    Ok(rows)
}

pub(crate) async fn set_key_quota_on<'e, E>(
    exec: E,
    user_id: &str,
    key_name: &str,
    micro_usd: i64,
) -> Result<(), StoreError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query(
        "INSERT INTO key_quotas (user_id, key_name, micro_usd, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(user_id, key_name) DO UPDATE SET micro_usd = ?3, updated_at = ?4",
    )
    .bind(user_id)
    .bind(key_name)
    .bind(micro_usd)
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(exec)
    .await?;
    Ok(())
}

pub(crate) async fn drop_key_quota_on<'e, E>(
    exec: E,
    user_id: &str,
    key_name: &str,
) -> Result<(), StoreError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("DELETE FROM key_quotas WHERE user_id = ?1 AND key_name = ?2")
        .bind(user_id)
        .bind(key_name)
        .execute(exec)
        .await?;
    Ok(())
}

#[cfg(test)]
mod pragma_tests {
    use super::*;

    /// 文件库必须跑在 WAL 上。
    ///
    /// 默认的 `delete` 模式下写事务会挡住所有读，而这个进程里有一个每轮都在写
    /// 的对账环和一堆每个请求都在读的接口 —— 症状是零星的 500，只在对账那一
    /// 刻出现，最难查的那种。断言 pragma 而不是去测时序：后者必然是 flaky 的。
    #[tokio::test]
    async fn 文件库跑在_wal_上_否则写会挡住所有读() {
        let path = std::env::temp_dir().join(format!("aisix-pragma-{}.db", uuid::Uuid::new_v4()));
        let s = Store::open(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        let (mode,): (String,) = sqlx::query_as("PRAGMA journal_mode")
            .fetch_one(s.pool())
            .await
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal", "日志模式不是 WAL");
        // 外键是真在生效的：删用户前必须先删他的流水与额度记录。
        let (fk,): (i64,) = sqlx::query_as("PRAGMA foreign_keys")
            .fetch_one(s.pool())
            .await
            .unwrap();
        assert_eq!(fk, 1, "外键没开，那些 REFERENCES 就只是注释");
        drop(s);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
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
