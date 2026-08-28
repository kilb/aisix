//! 注册、登录、会话。
//!
//! 三件事在这里是耦合的，因为它们共用同一个稀缺资源：argon2 的内存。
//!
//! - 口令用 argon2（与 `crates/aisix-console` 一致），不用 sha256：口令低熵，
//!   无盐无工作因子的散列被拖库后就是离线爆破。sha256 只配 API 密钥那种高熵
//!   随机串。
//! - argon2 默认单次占 **19 MiB**。注册与登录**都是未认证可达**的 argon2 操作，
//!   而门户面向陌生人 —— 不闸并发就是一条内存耗尽 DoS。两个端点共用一个信号量。
//! - 账号不存在时也要照样做一次校验。否则「口令错」慢、「无此账号」快，登录
//!   接口就成了账号枚举器 —— 返回体一致也挡不住计时侧信道。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use argon2::password_hash::{PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Argon2, PasswordHash};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rand_core::RngCore;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{RwLock, Semaphore};

use crate::store::{Store, StoreError};

/// 口令下限。低于此长度的口令挡在注册处，而不是留给用户自己负责。
const MIN_PASSWORD_LEN: usize = 12;
const SESSION_TTL_SECS: u64 = 12 * 3600;
/// 同一个邮箱连续失败多少次之后进入冷却。
const LOGIN_FAILURES_BEFORE_COOLDOWN: u32 = 5;
/// 铸密钥的频率窗口与窗口内上限。
///
/// 一分钟十把：正常用户一次建几把就够了，而这个数把「循环铸密钥逼网关一直重载」
/// 压到每分钟十次重载。
const MINT_WINDOW_SECS: u64 = 60;
const MINT_MAX_PER_WINDOW: u32 = 10;

/// 冷却时长。
///
/// 这个数是在两种坏结果之间选的：太长，别人连着输错五次就能把你锁在门外一整
/// 段时间（拿别人的邮箱刷失败即可）；太短，等于没拦。一分钟把爆破速度压到每
/// 分钟五次，而被误锁的人等一分钟就好。
const LOGIN_COOLDOWN_SECS: u64 = 60;
const COOKIE: &str = "aisix_portal";

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    sessions: Arc<RwLock<HashMap<String, (String, u64)>>>,
    /// 未认证可达的 argon2 操作的并发闸。见模块注释。
    gate: Arc<Semaphore>,
    /// 账号不存在时拿来「校验」的假散列，用于抹平计时差。
    dummy_hash: Arc<String>,
    /// 管理凭据。`None` = 未配置，此时管理端整个关闭（默认拒绝）。
    admin_token: Option<Arc<String>>,
    /// Prometheus 基址与网关配置文件路径。门户只**读**指标 —— 它绝不出现在
    /// 网关的请求路径上。
    prom_url: Arc<String>,
    resources: crate::resources::Writer,
    default_allowed_models: Arc<Vec<String>>,
    http: reqwest::Client,
    /// 按用户记的铸密钥时刻。见 [`AppState::mint_allowed`]。
    mints: Arc<RwLock<HashMap<String, Vec<u64>>>>,
    /// 按邮箱记的连续失败次数与冷却截止时刻。
    ///
    /// **按邮箱而不是按 IP**：门户在 nginx 后面，客户端 IP 要靠 `X-Forwarded-For`，
    /// 而那个头是可以伪造的 —— 除非明确信任前置代理，按它限流等于不限。邮箱是
    /// 攻击者绕不开的那一维：他要爆破某个账号，就必须一直用那个邮箱。
    ///
    /// 放内存里，跟会话一致：门户重启会清空，代价是重启那一刻的冷却被重置，
    /// 而收益是不给这条路径加一次数据库写。
    failures: Arc<RwLock<HashMap<String, (u32, u64)>>>,
    /// 实际执行过的 argon2 校验次数。让「不存在的账号也走了校验」这件事
    /// 可以被确定性地断言，而不必去测时间（那种测试必然是 flaky 的）。
    verifications: Arc<AtomicU64>,
}

impl AppState {
    pub fn new(store: Store, gate_permits: usize) -> Self {
        Self::with_admin_token(store, gate_permits, None)
    }

    pub fn with_admin_token(
        store: Store,
        gate_permits: usize,
        admin_token: Option<String>,
    ) -> Self {
        Self::build(
            store,
            gate_permits,
            admin_token,
            String::new(),
            String::new(),
        )
    }

    pub fn build(
        store: Store,
        gate_permits: usize,
        admin_token: Option<String>,
        prom_url: String,
        resources_path: String,
    ) -> Self {
        // 启动时算一次。内容无所谓，只要是一个合法的 argon2 PHC 串，
        // 校验它的开销与校验真散列同量级。
        let dummy =
            hash_password("aisix-portal-absent-account-placeholder").expect("生成占位散列失败");
        Self {
            store,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            gate: Arc::new(Semaphore::new(gate_permits)),
            dummy_hash: Arc::new(dummy),
            admin_token: admin_token.map(Arc::new),
            prom_url: Arc::new(prom_url),
            resources: crate::resources::Writer::new(resources_path),
            // 自助密钥默认能用哪些模型。按设计文档 §3.2，压住超支靠的是速率与
            // 模型白名单而不是对账周期 —— 这个默认值是运维手里那个旋钮。
            default_allowed_models: Arc::new(
                std::env::var("PORTAL_DEFAULT_ALLOWED_MODELS")
                    .map(|v| {
                        v.split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_else(|_| vec!["*".to_string()]),
            ),
            http: crate::client::outbound(),
            mints: Arc::new(RwLock::new(HashMap::new())),
            failures: Arc::new(RwLock::new(HashMap::new())),
            verifications: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 网关配置的读写闸口。两条写路径（铸密钥、对账停用）共用它。
    pub fn resources(&self) -> &crate::resources::Writer {
        &self.resources
    }

    pub fn default_allowed_models(&self) -> Vec<String> {
        (*self.default_allowed_models).clone()
    }

    /// 发一条**服务端构造**的 PromQL，取回一个标量。
    ///
    /// 这个方法故意只收已经拼好的查询串，且只有 `crate::usage` 里那几个模板会
    /// 调它 —— 门户不存在把调用方给的查询转发出去的路径。
    pub async fn prom_scalar(&self, promql: &str) -> Option<f64> {
        let url = format!("{}/api/v1/query", self.prom_url);
        let resp = self
            .http
            .get(&url)
            .query(&[("query", promql)])
            .send()
            .await
            .ok()?;
        let body: serde_json::Value = resp.json().await.ok()?;
        crate::usage::scalar_from_prom(&body)
    }

    pub fn admin_token(&self) -> Option<&str> {
        self.admin_token.as_deref().map(String::as_str)
    }

    /// 这个用户现在能不能再铸一把密钥。能，就把这次记下来。
    ///
    /// **限的是频率，不是总数。** 用户要的是「任意多把」，所以不设个数上限；但
    /// 每铸一把都要重写整份网关配置并发一次 SIGHUP，网关据此重建整个快照 ——
    /// 一个登录用户循环调这个接口，就能让网关一直在重载。
    ///
    /// 按用户记，所以谁也影响不到别人。
    pub async fn mint_allowed(&self, user_id: &str) -> bool {
        let now = now_secs();
        let mut m = self.mints.write().await;
        m.retain(|_, v| {
            v.retain(|t| *t + MINT_WINDOW_SECS > now);
            !v.is_empty()
        });
        let v = m.entry(user_id.to_string()).or_default();
        if v.len() as u32 >= MINT_MAX_PER_WINDOW {
            return false;
        }
        v.push(now);
        true
    }

    /// 这个邮箱现在是不是在冷却里。顺手清掉早就过期的记录。
    async fn in_cooldown(&self, email: &str) -> bool {
        let now = now_secs();
        let f = self.failures.read().await;
        matches!(f.get(email), Some((n, until)) if *n >= LOGIN_FAILURES_BEFORE_COOLDOWN && *until > now)
    }

    /// 记一次失败。
    ///
    /// 第二个数既是冷却截止时刻、也是这条记录的保鲜期，所以每次失败都往后推。
    /// 第一版只在到阈值时才设它，于是没到阈值的条目那个数是 0，下面的清理判据
    /// `0 + 窗口 > now` 恒假 —— 每次失败都把刚记的那条删掉，计数永远停在 1，
    /// 冷却根本不会触发。这条测试就是这么炸出来的。
    ///
    /// 副作用正好是想要的：失败之间隔得比窗口还长时，记录自然过期、计数归零 ——
    /// 慢速爆破本来也被压到每窗口五次。
    async fn note_failure(&self, email: &str) {
        let now = now_secs();
        let mut f = self.failures.write().await;
        f.retain(|_, (_, until)| *until > now);
        let e = f.entry(email.to_string()).or_insert((0, 0));
        e.0 += 1;
        e.1 = now + LOGIN_COOLDOWN_SECS;
    }

    /// 登录成功后清掉计数 —— 否则一个人偶尔输错几次，攒够五次就被锁。
    async fn clear_failures(&self, email: &str) {
        self.failures.write().await.remove(email);
    }

    /// 当前会话对应的 `user_id`，未登录或账号已停用则 `None`。顺手清掉过期项。
    ///
    /// **账号状态每次都查。** 登录处挡住了停用账号，但会话签发之后那个字段可能
    /// 才被改；只在登录处看的话，一个刚被停用的人还能拿着旧会话继续铸密钥、设
    /// 额度、提充值单，最长 12 小时。这里是每个需要登录的接口都要过的唯一路口，
    /// 所以检查放在这里，而不是散到各个 handler 里（散着写迟早漏一个）。
    pub async fn session_user(&self, headers: &HeaderMap) -> Option<String> {
        let tok = cookie_token(headers)?;
        let now = now_secs();
        // 读路径只取读锁，并只看这一条自己的过期时间。
        //
        // 原先每个请求都取**写**锁、再全表扫一遍清过期项 —— 一个读多写少的操作
        // 走独占锁，所有登录请求在这里排成一队。清理改成按量触发（见下），日常
        // 请求只读。
        let uid = {
            let s = self.sessions.read().await;
            match s.get(&tok) {
                Some((uid, exp)) if *exp > now => Some(uid.clone()),
                _ => None,
            }
        };
        // 没命中才需要考虑清理：要么这条过期了，要么根本不存在。顺手把攒下的
        // 过期项清掉 —— 这时本来就要写锁，不额外增加争用。
        let uid = match uid {
            Some(uid) => uid,
            None => {
                let mut s = self.sessions.write().await;
                s.retain(|_, (_, exp)| *exp > now);
                return None;
            }
        };
        match self.store.user_by_id(&uid).await {
            Ok(Some(u)) if !u.disabled => Some(uid),
            // 读不到就当没登录：这一步失败时放行等于把停用检查跳过去。
            _ => None,
        }
    }

    /// 当前会话条目数。测试用来断言过期项确实被清掉了。
    #[cfg(test)]
    pub async fn session_count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// 直接塞一条会话，指定过期时刻。测试用来造「已过期」的条目。
    #[cfg(test)]
    pub async fn put_session(&self, token: &str, user_id: &str, expires_at: u64) {
        self.sessions
            .write()
            .await
            .insert(token.to_string(), (user_id.to_string(), expires_at));
    }

    #[cfg(test)]
    pub fn verification_count(&self) -> u64 {
        self.verifications.load(Ordering::SeqCst)
    }
}

#[derive(Deserialize)]
pub struct RegisterReq {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

/// `POST /api/register`
pub async fn register(State(st): State<AppState>, Json(req): Json<RegisterReq>) -> Response {
    // 注册跟登录一样是未认证可达的 argon2 操作，同样要闸。
    let Ok(_permit) = st.gate.try_acquire() else {
        return busy();
    };

    let email = req.email.trim().to_ascii_lowercase();
    if !plausible_email(&email) {
        return bad_request("邮箱格式不正确");
    }
    if req.password.chars().count() < MIN_PASSWORD_LEN {
        return bad_request(&format!("口令至少 {MIN_PASSWORD_LEN} 个字符"));
    }

    let pw = req.password.clone();
    let hash = match tokio::task::spawn_blocking(move || hash_password(&pw)).await {
        Ok(Ok(h)) => h,
        Ok(Err(_)) | Err(_) => return server_error("口令散列失败"),
    };

    let id = uuid::Uuid::new_v4().to_string();
    match st
        .store
        .insert_user(&id, &email, &hash, req.display_name.as_deref())
        .await
    {
        Ok(()) => (StatusCode::CREATED, Json(json!({"user_id": id}))).into_response(),
        // 唯一约束冲突是可预期的用户错误，不是服务端故障。
        Err(StoreError::EmailTaken) => {
            (StatusCode::CONFLICT, Json(json!({"error": "邮箱已被注册"}))).into_response()
        }
        Err(_) => server_error("写入失败"),
    }
}

#[derive(Deserialize)]
pub struct LoginReq {
    pub email: String,
    pub password: String,
}

/// `POST /api/login`
pub async fn login(State(st): State<AppState>, Json(req): Json<LoginReq>) -> Response {
    let Ok(_permit) = st.gate.try_acquire() else {
        return busy();
    };

    let email = req.email.trim().to_ascii_lowercase();

    // 连续失败太多次就先冷却。**在跑 argon2 之前挡**，否则爆破照样能让每次尝试
    // 都吃掉一次 19 MiB 的散列计算。
    if st.in_cooldown(&email).await {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": "尝试过于频繁，请稍后再试"})),
        )
            .into_response();
    }

    let found = match st.store.user_by_email(&email).await {
        Ok(u) => u,
        Err(_) => return server_error("读取失败"),
    };

    // 关键：账号不存在时**也**跑一次 argon2，对着占位散列。
    // 少了这一步，「无此账号」会明显更快返回，返回体再怎么一致都白搭。
    let (hash, expect_id) = match &found {
        Some(u) if !u.disabled => (u.password_hash.clone(), Some(u.id.clone())),
        // 被停用的账号也走完整校验路径，不让它在时间上暴露出来。
        Some(u) => (u.password_hash.clone(), None),
        None => ((*st.dummy_hash).clone(), None),
    };

    let pw = req.password.clone();
    st.verifications.fetch_add(1, Ordering::SeqCst);
    let ok = tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash)
            .map(|parsed| {
                Argon2::default()
                    .verify_password(pw.as_bytes(), &parsed)
                    .is_ok()
            })
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false);

    let Some(uid) = expect_id.filter(|_| ok) else {
        st.note_failure(&email).await;
        // 口令错、账号不存在、账号被停用 —— 三者返回完全一致。
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "邮箱或口令不正确"})),
        )
            .into_response();
    };
    // 成功就清计数：一个人偶尔输错几次，攒够阈值就被锁在门外才是更常见的伤害。
    st.clear_failures(&email).await;

    let tok = random_token();
    st.sessions
        .write()
        .await
        .insert(tok.clone(), (uid, now_secs() + SESSION_TTL_SECS));
    // HttpOnly + SameSite=Strict：会话不能被别的站点借用。
    // Secure 是因为门户只经 HTTPS 暴露。
    let cookie = format!(
        "{COOKIE}={tok}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={SESSION_TTL_SECS}"
    );
    ([(header::SET_COOKIE, cookie)], Json(json!({"ok": true}))).into_response()
}

/// `POST /api/logout`
pub async fn logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(tok) = cookie_token(&headers) {
        st.sessions.write().await.remove(&tok);
    }
    let cleared = format!("{COOKIE}=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0");
    ([(header::SET_COOKIE, cleared)], Json(json!({"ok": true}))).into_response()
}

/// `GET /api/session`
pub async fn session(State(st): State<AppState>, headers: HeaderMap) -> Response {
    match st.session_user(&headers).await {
        Some(uid) => match st.store.user_by_id(&uid).await {
            Ok(Some(u)) => Json(json!({
                "authed": true,
                "user_id": u.id,
                "email": u.email,
                "display_name": u.display_name,
            }))
            .into_response(),
            // 会话有效但用户没了：当作未登录，不要漏出那个 id。
            _ => Json(json!({"authed": false})).into_response(),
        },
        None => Json(json!({"authed": false})).into_response(),
    }
}

fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    let salt = SaltString::generate(&mut rand_core::OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)?
        .to_string())
}

fn random_token() -> String {
    let mut b = [0u8; 32];
    rand_core::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| {
            c.split(';')
                .filter_map(|kv| kv.trim().split_once('='))
                .find(|(k, _)| *k == COOKIE)
                .map(|(_, v)| v.to_string())
        })
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 只做最低限度的形状检查。真正的可达性由邮箱验证流程负责（一期不做，
/// 见计划裁决 4），这里挡掉的是明显不是邮箱的输入。
fn plausible_email(s: &str) -> bool {
    let mut parts = s.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

fn busy() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({"error": "校验繁忙，请稍后重试"})),
    )
        .into_response()
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response()
}

fn server_error(msg: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": msg})),
    )
        .into_response()
}
