//! 通过真实控制面验证登录和注册是否启用了认证配置。

mod support;

use std::time::Duration;

use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::members::SIGNUP_LOCK_KEY;
use ring::hmac;
use serde_json::{json, Value};
use support::{TestCp, TEST_MASTER_KEY, TEST_PASSWORD};
use uuid::Uuid;

fn auth_config(email: Value, turnstile: Value, origin: &str, version: u64) -> Value {
    json!({
        "version": version, "email": email, "turnstile": turnstile,
        "passkey": {"enabled": false},
        "github": {"enabled": false, "client_id": ""},
        "google": {"enabled": false, "client_id": ""},
        "portal_origin": origin,
    })
}

fn email_disabled() -> Value {
    json!({"enabled": false, "endpoint": "", "from": ""})
}

fn email_enabled(endpoint: &str) -> Value {
    json!({"enabled": true, "endpoint": endpoint, "from": "sender@example.com", "api_key": "test-send-key"})
}

/// 注入一封已送达邮件在 PostgreSQL 中的状态，测试注册消费，不需要真实邮件服务凭据。
async fn seed_delivered_code(cp: &TestCp, email: &str, code: &str, endpoint: &str, age_min: i32) {
    let id = Uuid::new_v4();
    let key = hmac::Key::new(hmac::HMAC_SHA256, &TEST_MASTER_KEY);
    let mailbox = hmac::sign(
        &key,
        &[b"aisix.portal.signup.recipient.v1\0", email.as_bytes()].concat(),
    );
    let proof = hmac::sign(
        &key,
        &[
            b"aisix.portal.signup.code.v1\0".as_slice(),
            id.as_bytes(),
            code.as_bytes(),
            endpoint.as_bytes(),
            b"\0sender@example.com\0test-send-key",
        ]
        .concat(),
    );
    sqlx::query(
        "INSERT INTO portal_email_codes (id, recipient_mac, code_mac, issued_at, delivered)
         VALUES ($1, $2, $3, statement_timestamp() - make_interval(mins => $4), true)",
    )
    .bind(id)
    .bind(mailbox.as_ref())
    .bind(proof.as_ref())
    .bind(age_min)
    .execute(cp.db().pool())
    .await
    .expect("邮件已可信地送达用户后的数据库记录");
}

async fn register_with_code(cp: &TestCp, email: &str, code: &str) -> support::Resp {
    cp.send("POST", "/api/signup", None, Some(json!({
        "org_name": "邮箱认证组织", "email": email, "password": TEST_PASSWORD, "email_code": code,
    }))).await
}

#[tokio::test]
async fn enabled_email_verification_requires_a_valid_code_before_signup() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let configured = cp
        .send(
            "PUT",
            "/admin/settings/authentication",
            Some(admin.cookie()),
            Some(auth_config(
                email_enabled("https://example.com/emails"),
                json!({"enabled": false, "site_key": ""}),
                "https://portal.example.com",
                0,
            )),
        )
        .await;
    assert_eq!(configured.status, 200, "{configured:?}");
    let before = cp.count("members").await;
    for code in [None, Some("012345")] {
        let mut body = json!({
            "org_name": "邮箱认证组织", "email": "user@example.com", "password": TEST_PASSWORD,
        });
        if let Some(code) = code {
            body["email_code"] = json!(code);
        }
        let denied = cp.send("POST", "/api/signup", None, Some(body)).await;
        assert_eq!(denied.status, 422, "{denied:?}");
        assert_eq!(denied.body["details"][0]["path"], "/email_code");
        assert_eq!(cp.count("members").await, before);
    }
}

#[tokio::test]
async fn a_signup_in_flight_cannot_bypass_the_first_enable_of_email_verification() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let before = cp.count("members").await;
    let mut fence = cp.db().pool().begin().await.expect("transaction");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SIGNUP_LOCK_KEY)
        .execute(&mut *fence)
        .await
        .expect("hold registration before it can write");

    let enable_while_queued = async {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory'
                     AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
                     AND NOT granted AND classid::bigint = ($1::bigint >> 32)
                     AND objid::bigint = ($1::bigint & 4294967295::bigint))",
                )
                .bind(SIGNUP_LOCK_KEY)
                .fetch_one(cp.db().pool())
                .await
                .expect("read the registration lock queue");
                if waiting {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("registration reached its write boundary");
        let enabled = cp
            .send(
                "PUT",
                "/admin/settings/authentication",
                Some(admin.cookie()),
                Some(auth_config(
                    email_enabled("https://example.com/emails"),
                    json!({"enabled": false, "site_key": ""}),
                    "https://portal.example.com",
                    0,
                )),
            )
            .await;
        assert_eq!(enabled.status, 200, "{enabled:?}");
        fence.commit().await.expect("resume the registered request");
    };
    let (denied, ()) = tokio::join!(
        cp.send("POST", "/api/signup", None, Some(json!({
            "org_name": "验证切换组织", "email": "inflight@example.com", "password": TEST_PASSWORD,
        }))),
        enable_while_queued,
    );
    assert_eq!(denied.status, 409, "{denied:?}");
    assert_eq!(denied.body["code"], "authentication_settings_changed");
    assert!(denied.set_cookie.is_none());
    assert_eq!(cp.count("members").await, before);
}

#[tokio::test]
async fn enabled_turnstile_requires_a_challenge_before_password_login() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let configured = cp
        .send(
            "PUT",
            "/admin/settings/authentication",
            Some(admin.cookie()),
            Some(auth_config(
                email_disabled(),
                json!({"enabled": true, "site_key": "some-real-site-key", "secret_key": "some-real-secret"}),
                "https://portal.example.com",
                0,
            )),
        )
        .await;
    assert_eq!(configured.status, 200, "{configured:?}");
    let denied = cp
        .send(
            "POST",
            "/api/login",
            None,
            Some(json!({"email": "user@example.com", "password": TEST_PASSWORD})),
        )
        .await;
    assert_eq!(denied.status, 422, "{denied:?}");
    assert_eq!(denied.body["details"][0]["path"], "/turnstile_token");
}

#[tokio::test]
async fn a_failed_email_delivery_counts_against_the_mailbox_cooldown() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let endpoint = "https://relay.invalid/emails";
    let configured = cp
        .send(
            "PUT",
            "/admin/settings/authentication",
            Some(admin.cookie()),
            Some(auth_config(
                email_enabled(endpoint),
                json!({"enabled": false, "site_key": ""}),
                "https://portal.example.com",
                0,
            )),
        )
        .await;
    assert_eq!(configured.status, 200, "{configured:?}");
    seed_delivered_code(&cp, "test@example.com", "041527", endpoint, 2).await;
    let unreachable = cp
        .send(
            "POST",
            "/api/auth/email-code",
            None,
            Some(json!({"email": "TEST@example.com"})),
        )
        .await;
    assert_eq!(unreachable.status, 503, "{unreachable:?}");
    let cooldown = cp
        .send(
            "POST",
            "/api/auth/email-code",
            None,
            Some(json!({"email": "test@example.com"})),
        )
        .await;
    assert_eq!(cooldown.status, 429, "{cooldown:?}");
    assert_eq!(cooldown.body["code"], "busy");
    let using_previous = register_with_code(&cp, "test@example.com", "041527").await;
    assert_eq!(using_previous.status, 201, "{using_previous:?}");
}

#[tokio::test]
async fn a_code_only_registers_its_own_mailbox_once_and_locks_after_five_guesses() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let endpoint = "https://example.com/emails";
    let configured = cp
        .send(
            "PUT",
            "/admin/settings/authentication",
            Some(admin.cookie()),
            Some(auth_config(
                email_enabled(endpoint),
                json!({"enabled": false, "site_key": ""}),
                "https://portal.example.com",
                0,
            )),
        )
        .await;
    assert_eq!(configured.status, 200, "{configured:?}");
    let before = cp.count("members").await;
    seed_delivered_code(&cp, "owner@example.com", "041527", endpoint, 0).await;
    seed_delivered_code(&cp, "blocked@example.com", "041528", endpoint, 0).await;
    seed_delivered_code(&cp, "expired@example.com", "041529", endpoint, 11).await;

    for (email, code) in [
        ("other@example.com", "041527"),
        ("expired@example.com", "041529"),
    ] {
        let denied = register_with_code(&cp, email, code).await;
        assert_eq!(denied.status, 422, "{denied:?}");
        assert_eq!(denied.body["details"][0]["path"], "/email_code");
    }
    for code in ["100000", "100001", "100002", "100003", "100004", "041528"] {
        let denied = register_with_code(&cp, "blocked@example.com", code).await;
        assert_eq!(denied.status, 422, "{denied:?}");
        assert_eq!(denied.body["details"][0]["path"], "/email_code");
    }
    assert_eq!(cp.count("members").await, before);

    let (first, second) = tokio::join!(
        register_with_code(&cp, "owner@example.com", "041527"),
        register_with_code(&cp, "OWNER@example.com", "041527"),
    );
    let (success, denied) = if first.status == 201 {
        (&first, &second)
    } else {
        (&second, &first)
    };
    assert_eq!(success.status, 201, "{first:?} {second:?}");
    assert!(success.set_cookie.is_some());
    assert_eq!(denied.status, 422, "{denied:?}");
    assert_eq!(denied.body["details"][0]["path"], "/email_code");
    assert_eq!(cp.count("members").await, before + 1);
}

/// 运行时需要连接官方 Siteverify 端点。CP_TEST_LIVE_TURNSTILE=1 cargo test ...
#[tokio::test]
async fn live_cloudflare_accepts_signup_and_login_and_denies_the_fail_challenge() {
    if std::env::var("CP_TEST_LIVE_TURNSTILE").as_deref() != Ok("1") {
        return;
    }
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    // 官方的通过、失败测试组件和服务端密钥。
    // https://developers.cloudflare.com/turnstile/troubleshooting/testing/
    let test_config = |site_key: &str, secret_key: &str, version| {
        auth_config(
            email_disabled(),
            json!({"enabled": true, "site_key": site_key, "secret_key": secret_key}),
            "http://localhost:4173",
            version,
        )
    };
    let enabled = cp
        .send(
            "PUT",
            "/admin/settings/authentication",
            Some(admin.cookie()),
            Some(test_config(
                "1x00000000000000000000AA",
                "1x0000000000000000000000000000000AA",
                0,
            )),
        )
        .await;
    assert_eq!(enabled.status, 200, "{enabled:?}");
    let signed_up = cp
        .send(
            "POST",
            "/api/signup",
            None,
            Some(json!({
                "org_name": "人机验证组织", "email": "user@example.com", "password": TEST_PASSWORD,
                "turnstile_token": "XXXX.DUMMY.TOKEN.XXXX",
            })),
        )
        .await;
    assert_eq!(signed_up.status, 201, "{signed_up:?}");
    assert!(signed_up.set_cookie.is_some());
    let login = json!({"email": "user@example.com", "password": TEST_PASSWORD, "turnstile_token": "XXXX.DUMMY.TOKEN.XXXX"});
    let signed_in = cp
        .send("POST", "/api/login", None, Some(login.clone()))
        .await;
    assert_eq!(signed_in.status, 200, "{signed_in:?}");
    assert!(signed_in.set_cookie.is_some());
    let rotated = cp
        .send(
            "PUT",
            "/admin/settings/authentication",
            Some(admin.cookie()),
            Some(test_config(
                "2x00000000000000000000AB",
                "2x0000000000000000000000000000000AA",
                1,
            )),
        )
        .await;
    assert_eq!(rotated.status, 200, "{rotated:?}");
    let denied = cp.send("POST", "/api/login", None, Some(login)).await;
    assert_eq!(denied.status, 422, "{denied:?}");
    assert_eq!(denied.body["details"][0]["path"], "/turnstile_token");
    assert!(denied.set_cookie.is_none());
}
