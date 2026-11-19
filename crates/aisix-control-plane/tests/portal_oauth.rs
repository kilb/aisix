//! 用真实控制面和 PostgreSQL 验证 OAuth 跳转与一次性回调。测试使用服务商的
//! 取消授权回调合同，不需要外部测试账号或模拟 OAuth 服务。

mod support;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::tenants::TenantStatus;
use reqwest::header::{CACHE_CONTROL, COOKIE, LOCATION, REFERRER_POLICY, SET_COOKIE};
use reqwest::{StatusCode, Url};
use serde_json::json;
use support::{TestCp, TEST_PASSWORD};

const SETTINGS: &str = "/admin/settings/authentication";
const START: &str = "/api/auth/oauth/github/start";

#[tokio::test]
async fn oauth_redirect_requires_enabled_provider_and_a_one_use_browser_state() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let login = json!({"mode": "login"});
    let disabled = cp.send("POST", START, None, Some(login.clone())).await;
    assert_eq!(disabled.status, 404, "disabled: {disabled:?}");

    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let configured = cp
        .send(
            "PUT",
            SETTINGS,
            Some(admin.cookie()),
            Some(json!({
                "version": 0,
                "portal_origin": "https://portal.example.test",
                "email": {"enabled":false,"endpoint":"","from":""},
                "turnstile": {"enabled":false,"site_key":""},
                "passkey": {"enabled":false},
                "github": {"enabled":true,"client_id":"test-client","client_secret":"test-secret"},
                "google": {"enabled":true,"client_id":"google-test-client","client_secret":"google-test-secret"}
            })),
        )
        .await;
    assert_eq!(configured.status, 200, "settings: {configured:?}");

    let started = cp.send("POST", START, None, Some(login)).await;
    assert_eq!(started.status, 200, "start: {started:?}");
    let url = Url::parse(started.body["url"].as_str().expect("授权 URL")).unwrap();
    assert_eq!(url.origin().ascii_serialization(), "https://github.com");
    assert_eq!(url.path(), "/login/oauth/authorize");
    let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(query["client_id"], "test-client");
    assert_eq!(
        query["redirect_uri"],
        "https://portal.example.test/api/auth/oauth/github/callback"
    );
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["code_challenge"].len(), 43);
    let state = &query["state"];
    assert!(!state.is_empty());
    assert!(!url.as_str().contains("test-secret"));
    let cookie = started.set_cookie.as_deref().expect("短期浏览器 Cookie");
    assert!(cookie.starts_with("__Host-"));
    assert!(cookie.contains("; HttpOnly; Secure; SameSite=Lax; Path=/;"));
    let cookie_pair = cookie.split(';').next().unwrap();
    assert!(!cookie_pair.contains(&**state));

    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let callback = cp.url(&format!(
        "/api/auth/oauth/github/callback?error=access_denied&state={state}"
    ));
    let missing_cookie = client.get(&callback).send().await.unwrap();
    assert_eq!(missing_cookie.status(), StatusCode::SEE_OTHER);
    assert_eq!(missing_cookie.headers()[LOCATION], "/?auth=error");
    let flows = || {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM portal_oauth_flows")
            .fetch_one(cp.db().pool())
    };
    assert_eq!(flows().await.unwrap(), 1);

    let cross_provider = cp.url(&format!(
        "/api/auth/oauth/google/callback?error=access_denied&state={state}"
    ));
    let mixed = client
        .get(cross_provider)
        .header(COOKIE, cookie_pair.replacen("github", "google", 1))
        .send()
        .await
        .unwrap();
    assert_eq!(mixed.headers()[LOCATION], "/?auth=error");
    assert_eq!(flows().await.unwrap(), 1);

    let denied = client
        .get(&callback)
        .header(COOKIE, cookie_pair)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::SEE_OTHER);
    assert_eq!(denied.headers()[LOCATION], "/?auth=error");
    assert_eq!(denied.headers()[CACHE_CONTROL], "no-store");
    assert_eq!(denied.headers()[REFERRER_POLICY], "no-referrer");
    assert!(denied.headers()[SET_COOKIE]
        .to_str()
        .unwrap()
        .contains("Max-Age=0"));
    assert_eq!(flows().await.unwrap(), 0);

    let replay = client
        .get(&callback)
        .header(COOKIE, cookie_pair)
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::SEE_OTHER);
    assert!(!replay.headers().contains_key(SET_COOKIE));

    let google = cp
        .send(
            "POST",
            "/api/auth/oauth/google/start",
            None,
            Some(json!({"mode":"login"})),
        )
        .await;
    assert_eq!(google.status, 200, "start: {google:?}");
    let url = Url::parse(google.body["url"].as_str().expect("Google 授权 URL")).unwrap();
    assert_eq!(
        url.origin().ascii_serialization(),
        "https://accounts.google.com"
    );
    assert_eq!(url.path(), "/o/oauth2/v2/auth");
    let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(query["client_id"], "google-test-client");
    assert_eq!(
        query["redirect_uri"],
        "https://portal.example.test/api/auth/oauth/google/callback"
    );
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["scope"], "openid");
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["nonce"].len(), 43);
    assert_ne!(query["state"], query["nonce"]);
    assert!(!url.as_str().contains("google-test-secret"));
}

#[tokio::test]
async fn an_oauth_link_is_private_and_requires_password_to_remove() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let holder = cp.seed_member(tenant, MemberRole::Owner).await;
    let other = cp.seed_member(tenant, MemberRole::Member).await;
    let holder = cp.member_login_as(&holder).await;
    let other = cp.member_login_as(&other).await;
    sqlx::query("INSERT INTO member_oauth_identities (member_id, provider, subject) VALUES ($1, 'github', '913005')")
        .bind(holder.id).execute(cp.db().pool()).await.unwrap();

    let linked = cp.get_with_cookie("/api/auth/oauth", holder.cookie()).await;
    assert_eq!(linked.status, 200, "linked: {linked:?}");
    assert_eq!(linked.body, json!({"github":true,"google":false}));
    let unrelated = cp.get_with_cookie("/api/auth/oauth", other.cookie()).await;
    assert_eq!(unrelated.body, json!({"github":false,"google":false}));
    let path = "/api/auth/oauth/github";
    let bad = cp
        .send(
            "DELETE",
            path,
            Some(holder.cookie()),
            Some(json!({"password":"wrong password"})),
        )
        .await;
    assert_eq!(bad.status, 401, "bad password: {bad:?}");
    let unlinked = cp
        .send(
            "DELETE",
            path,
            Some(holder.cookie()),
            Some(json!({"password": TEST_PASSWORD})),
        )
        .await;
    assert_eq!(unlinked.status, 204, "unlink: {unlinked:?}");
    let current = cp.get_with_cookie("/api/auth/oauth", holder.cookie()).await;
    assert_eq!(current.body, json!({"github":false,"google":false}));
}
