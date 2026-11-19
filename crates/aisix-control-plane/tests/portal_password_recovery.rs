//! 用真实控制面和 PostgreSQL 验证租户口令找回。

mod support;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::tenants::TenantStatus;
use ring::hmac;
use serde_json::json;
use support::{TestCp, TEST_MASTER_KEY, TEST_PASSWORD};
use uuid::Uuid;

const ENDPOINT: &str = "https://relay.invalid/emails";
const SENDER: &str = "sender@example.com";
const TEST_RELAY_KEY: &str = "test-relay-key";
const CODE: &str = "038416";
const NEW_PASSWORD: &str = "new portal recovery password";

async fn configure_mail(cp: &TestCp, admin: &str, turnstile: bool, recovery: Option<bool>) {
    let current = cp
        .get_with_cookie("/admin/settings/authentication", admin)
        .await;
    let challenge = if turnstile {
        json!({"enabled": true, "site_key": "1x00000000000000000000AA",
               "secret_key": "1x0000000000000000000000000000000AA"})
    } else {
        json!({"enabled": false, "site_key": ""})
    };
    let mut body = json!({
        "version": current.body["version"],
        "portal_origin": "https://localhost:5173",
        "email": {"enabled": true, "endpoint": ENDPOINT, "from": SENDER, "api_key": TEST_RELAY_KEY},
        "turnstile": challenge,
        "passkey": {"enabled": false},
        "github": {"enabled": false, "client_id": ""},
        "google": {"enabled": false, "client_id": ""},
    });
    if let Some(enabled) = recovery {
        body["password_recovery_enabled"] = json!(enabled);
    }
    let result = cp
        .send(
            "PUT",
            "/admin/settings/authentication",
            Some(admin),
            Some(body),
        )
        .await;
    assert_eq!(result.status, 200, "{result:?}");
}

async fn forgot(cp: &TestCp, email: &str) -> support::Resp {
    cp.send(
        "POST",
        "/api/auth/password/forgot",
        None,
        Some(json!({"email": email})),
    )
    .await
}

async fn reset(cp: &TestCp, email: &str, code: &str) -> support::Resp {
    cp.send(
        "POST",
        "/api/auth/password/reset",
        None,
        Some(json!({"email": email, "email_code": code, "new_password": NEW_PASSWORD})),
    )
    .await
}

/// 注入唯一来自外部邮件服务的前提，随后通过公开接口消费该验证码。
async fn seed_delivered_code(cp: &TestCp, member: Uuid, email: &str, age_min: i32) {
    let id = Uuid::new_v4();
    let password_hash: String =
        sqlx::query_scalar("SELECT password_hash FROM members WHERE id = $1")
            .bind(member)
            .fetch_one(cp.db().pool())
            .await
            .expect("成员已有的口令");
    let key = hmac::Key::new(hmac::HMAC_SHA256, &TEST_MASTER_KEY);
    let auth_version: i64 = sqlx::query_scalar(
        "SELECT (value->'config'->>'version')::bigint FROM cp_settings WHERE key = 'authentication'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("当前认证配置版本");
    let recipient = hmac::sign(
        &key,
        &[b"aisix.portal.recovery.recipient.v1\0", email.as_bytes()].concat(),
    );
    let proof = hmac::sign(
        &key,
        &[
            b"aisix.portal.recovery.code.v1\0".as_slice(),
            id.as_bytes(),
            member.as_bytes(),
            &auth_version.to_be_bytes(),
            CODE.as_bytes(),
            password_hash.as_bytes(),
        ]
        .concat(),
    );
    sqlx::query(
        "INSERT INTO portal_password_recovery (id, recipient_mac, member_id, code_mac, issued_at)
         VALUES ($1, $2, $3, $4, statement_timestamp() - make_interval(mins => $5))",
    )
    .bind(id)
    .bind(recipient.as_ref())
    .bind(member)
    .bind(proof.as_ref())
    .bind(age_min)
    .execute(cp.db().pool())
    .await
    .expect("邮件已送达后的数据库记录");
}

#[tokio::test]
async fn password_recovery_requires_email_and_returns_the_same_public_response() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let options = cp.send("GET", "/api/auth/options", None, None).await;
    assert_eq!(options.body["password_recovery"], false, "{options:?}");
    assert_eq!(forgot(&cp, "anyone@example.com").await.status, 404);
    assert_eq!(reset(&cp, "anyone@example.com", CODE).await.status, 404);

    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let pending = cp.seed_tenant(TenantStatus::Pending).await;
    let enabled = cp.seed_member(pending, MemberRole::Owner).await;
    let disabled = cp.seed_member(pending, MemberRole::Member).await;
    sqlx::query("UPDATE members SET disabled = true WHERE id = $1")
        .bind(disabled.id)
        .execute(cp.db().pool())
        .await
        .expect("停用成员");
    configure_mail(&cp, admin.cookie(), false, None).await;
    let email_only = cp.send("GET", "/api/auth/options", None, None).await;
    assert_eq!(
        email_only.body["email_verification"], true,
        "{email_only:?}"
    );
    assert_eq!(
        email_only.body["password_recovery"], false,
        "{email_only:?}"
    );
    assert_eq!(forgot(&cp, &enabled.email).await.status, 404);
    configure_mail(&cp, admin.cookie(), false, Some(true)).await;
    let options = cp.send("GET", "/api/auth/options", None, None).await;
    assert_eq!(options.body["password_recovery"], true, "{options:?}");
    for email in [
        &enabled.email,
        &disabled.email,
        "absent@example.com",
        &enabled.email,
    ] {
        let response = forgot(&cp, email).await;
        assert_eq!(response.status, 204, "{response:?}");
        assert!(response.body.is_null());
        assert!(response.headers.get("set-cookie").is_none());
    }
    assert_eq!(
        cp.count("portal_password_recovery").await,
        3,
        "同一邮箱的两次请求只占用一条预留记录"
    );
    configure_mail(&cp, admin.cookie(), true, None).await;
    let response = forgot(&cp, "challenge@example.com").await;
    assert_eq!(response.status, 422, "{response:?}");
    assert_eq!(response.body["details"][0]["path"], "/turnstile_token");
}

#[tokio::test]
async fn completed_recovery_changes_only_one_password_and_revokes_only_its_sessions() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Pending).await;
    let member = cp.seed_member(tenant, MemberRole::Owner).await;
    let same_tenant = cp.member_login_seeded(tenant, MemberRole::Member).await;
    let first = cp.member_login_as(&member).await;
    let second = cp.member_login_as(&member).await;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    configure_mail(&cp, admin.cookie(), false, Some(true)).await;
    seed_delivered_code(&cp, member.id, &member.email, 0).await;

    let wrong_mailbox = reset(&cp, &same_tenant.email, CODE).await;
    assert_eq!(wrong_mailbox.status, 422, "{wrong_mailbox:?}");
    let (first_reset, second_reset) = tokio::join!(
        reset(&cp, &member.email, CODE),
        reset(&cp, &member.email, CODE),
    );
    let mut statuses = [first_reset.status, second_reset.status];
    statuses.sort();
    assert_eq!(statuses, [204, 422], "{first_reset:?}, {second_reset:?}");
    for previous in [first, second] {
        let session = cp.get_with_cookie("/api/session", previous.cookie()).await;
        assert_eq!(session.body["authed"], false, "{session:?}");
    }
    let others = cp
        .get_with_cookie("/api/session", same_tenant.cookie())
        .await;
    assert_eq!(others.body["authed"], true, "{others:?}");
    assert_eq!(
        cp.get_with_cookie("/admin/settings/authentication", admin.cookie())
            .await
            .status,
        200
    );
    assert_eq!(
        cp.member_login(&member.email, TEST_PASSWORD).await.status,
        401
    );
    assert_eq!(
        cp.member_login(&member.email, NEW_PASSWORD).await.status,
        200
    );
    assert_eq!(
        reset(&cp, &member.email, CODE).await.status,
        422,
        "同一验证码只能使用一次"
    );
}

#[tokio::test]
async fn expired_exhausted_and_ineligible_codes_all_fail_with_the_same_field_error() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let expired = cp.seed_member(tenant, MemberRole::Owner).await;
    let exhausted = cp.seed_member(tenant, MemberRole::Member).await;
    let disabled = cp.seed_member(tenant, MemberRole::Member).await;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    configure_mail(&cp, admin.cookie(), false, Some(true)).await;

    seed_delivered_code(&cp, expired.id, &expired.email, 11).await;
    seed_delivered_code(&cp, exhausted.id, &exhausted.email, 0).await;
    seed_delivered_code(&cp, disabled.id, &disabled.email, 0).await;
    sqlx::query("UPDATE members SET disabled = true WHERE id = $1")
        .bind(disabled.id)
        .execute(cp.db().pool())
        .await
        .expect("签发验证码之后停用成员");
    let unknown = reset(&cp, "nobody@example.com", CODE).await;
    assert_eq!(unknown.status, 422, "{unknown:?}");
    assert_eq!(unknown.body["details"][0]["path"], "/email_code");
    for _ in 0..5 {
        let wrong = reset(&cp, &exhausted.email, "871623").await;
        assert_eq!(wrong.status, 422, "{wrong:?}");
        assert_eq!(wrong.body, unknown.body);
    }
    for mail in [&expired.email, &exhausted.email, &disabled.email] {
        let refused = reset(&cp, mail, CODE).await;
        assert_eq!(refused.status, 422, "{refused:?}");
        assert_eq!(refused.body, unknown.body);
    }
    let unchanged = cp.member_login(&exhausted.email, TEST_PASSWORD).await;
    assert_eq!(unchanged.status, 200, "{unchanged:?}");
}

#[tokio::test]
async fn re_enabling_recovery_does_not_reactivate_previously_issued_codes() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Pending).await;
    let member = cp.seed_member(tenant, MemberRole::Owner).await;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    configure_mail(&cp, admin.cookie(), false, Some(true)).await;
    seed_delivered_code(&cp, member.id, &member.email, 0).await;

    configure_mail(&cp, admin.cookie(), false, Some(false)).await;
    let disabled = reset(&cp, &member.email, CODE).await;
    assert_eq!(disabled.status, 404, "{disabled:?}");
    configure_mail(&cp, admin.cookie(), false, Some(true)).await;
    let old_code = reset(&cp, &member.email, CODE).await;
    assert_eq!(old_code.status, 422, "{old_code:?}");
    assert_eq!(old_code.body["details"][0]["path"], "/email_code");
    let unchanged = cp.member_login(&member.email, TEST_PASSWORD).await;
    assert_eq!(unchanged.status, 200, "{unchanged:?}");
}
