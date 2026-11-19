mod support;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::tenants::TenantStatus;
use serde_json::{json, Value};
use support::{TestCp, TEST_PASSWORD};

const SETTINGS: &str = "/admin/settings/authentication";
const OPTIONS: &str = "/api/auth/options";
const MAIL_KEY: &str = "mail-secret-for-auth-settings";
const GITHUB_SECRET: &str = "github-secret-for-auth-settings";
const MAIL_URL_PASSWORD: &str = "mail-url-password";

fn settings() -> Value {
    json!({
        "version": 0,
        "portal_origin": "https://portal.example.com/",
        "email": {
            "enabled": true,
            "endpoint": format!("https://mail:{MAIL_URL_PASSWORD}@mail.example.com/v1/send"),
            "from": "admin@example.com",
            "api_key": MAIL_KEY,
        },
        "turnstile": {"enabled": false, "site_key": ""},
        "passkey": {"enabled": true},
        "github": {"enabled": true, "client_id": "github-client", "client_secret": GITHUB_SECRET},
        "google": {"enabled": false, "client_id": ""}
    })
}

#[tokio::test]
async fn authentication_settings_activate_independently_and_preserve_secrets_on_edit() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let denied = cp.send("GET", SETTINGS, Some(viewer.cookie()), None).await;
    assert_eq!(denied.status, 403, "viewer: {denied:?}");
    let options = cp.send("GET", OPTIONS, None, None).await;
    assert_eq!(options.status, 200, "public options: {options:?}");
    assert_eq!(
        options.body,
        json!({
            "email_verification": false, "password_recovery": false, "turnstile_site_key": null,
            "passkey": false, "github": false, "google": false,
        })
    );

    let saved = cp
        .send("PUT", SETTINGS, Some(admin.cookie()), Some(settings()))
        .await;
    assert_eq!(saved.status, 200, "save settings: {saved:?}");
    assert_eq!(saved.body["version"], 1);
    assert_eq!(saved.body["portal_origin"], "https://portal.example.com");
    assert_eq!(saved.body["email"]["api_key_configured"], true);
    assert_eq!(
        saved.body["email"]["endpoint"],
        "https://***@mail.example.com/v1/send"
    );
    assert_eq!(
        saved.body["github"]["callback_url"],
        "https://portal.example.com/api/auth/oauth/github/callback"
    );
    assert_eq!(saved.body["github"]["client_secret_configured"], true);
    let loaded = cp.send("GET", SETTINGS, Some(admin.cookie()), None).await;
    assert_eq!(loaded.status, 200, "admin reads settings: {loaded:?}");
    assert_eq!(loaded.body, saved.body);
    let public = cp.send("GET", OPTIONS, None, None).await;
    assert_eq!(public.status, 200, "public options: {public:?}");
    assert_eq!(
        public.body,
        json!({
            "email_verification": true, "password_recovery": false, "turnstile_site_key": null,
            "passkey": true, "github": true, "google": false,
        })
    );

    let mut edit = settings();
    edit["version"] = saved.body["version"].clone();
    edit["email"]
        .as_object_mut()
        .expect("email object")
        .remove("api_key");
    edit["email"]["endpoint"] = saved.body["email"]["endpoint"].clone();
    edit["github"]
        .as_object_mut()
        .expect("github object")
        .remove("client_secret");
    edit["passkey"]["enabled"] = false.into();
    let mut moved = edit.clone();
    moved["email"]["endpoint"] = "https://other.example.com/send".into();
    let refused = cp
        .send("PUT", SETTINGS, Some(admin.cookie()), Some(moved))
        .await;
    assert_eq!(
        refused.status, 422,
        "moving a stored key to a different endpoint: {refused:?}"
    );
    let edited = cp
        .send("PUT", SETTINGS, Some(admin.cookie()), Some(edit))
        .await;
    assert_eq!(edited.status, 200, "retain existing secrets: {edited:?}");
    assert_eq!(edited.body["version"], 2);
    assert_eq!(edited.body["email"]["api_key_configured"], true);
    assert_eq!(edited.body["github"]["client_secret_configured"], true);
    assert_eq!(edited.body["passkey"]["enabled"], false);
    let stale = cp
        .send("PUT", SETTINGS, Some(admin.cookie()), Some(settings()))
        .await;
    assert_eq!(stale.status, 412, "stale update: {stale:?}");
    assert_eq!(stale.body["code"], "version_mismatch");
    assert_eq!(stale.body["current_version"], 2);
    let current = cp.send("GET", SETTINGS, Some(admin.cookie()), None).await;
    assert_eq!(current.body, edited.body);

    let serialized = sqlx::query_scalar::<_, String>(
        "SELECT value::text FROM cp_settings WHERE key = 'authentication'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("the stored row");
    let audit = cp
        .send("GET", "/admin/audit", Some(admin.cookie()), None)
        .await;
    assert_eq!(audit.status, 200, "audit log: {audit:?}");
    for value in [
        serialized,
        saved.body.to_string(),
        loaded.body.to_string(),
        edited.body.to_string(),
        audit.body.to_string(),
    ] {
        assert!(!value.contains(MAIL_KEY), "mail API key leaked");
        assert!(
            !value.contains(MAIL_URL_PASSWORD),
            "mail URL userinfo leaked"
        );
        assert!(!value.contains(GITHUB_SECRET), "GitHub OAuth secret leaked");
    }
}

#[tokio::test]
async fn authentication_settings_reject_invalid_values_without_activating_features() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let mut invalid = settings();
    invalid["portal_origin"] = "https://portal.example.com/app".into();
    invalid["email"]["endpoint"] = "https://[::ffff:127.0.0.1]/send".into();
    invalid["turnstile"]["enabled"] = true.into();
    let response = cp
        .send("PUT", SETTINGS, Some(admin.cookie()), Some(invalid))
        .await;
    assert_eq!(response.status, 422, "invalid config: {response:?}");
    let problems = response.body["details"]
        .as_array()
        .expect("validation details");
    for path in [
        "/portal_origin",
        "/email/endpoint",
        "/turnstile/site_key",
        "/turnstile/secret_key",
    ] {
        assert!(
            problems.iter().any(|d| d["path"] == path),
            "missing {path}: {response:?}"
        );
    }
    let public = cp.send("GET", OPTIONS, None, None).await;
    assert_eq!(public.status, 200, "public options: {public:?}");
    assert_eq!(public.body["email_verification"], false);
    assert_eq!(public.body["github"], false);
    assert_eq!(public.body["passkey"], false);
}

#[tokio::test]
async fn oauth_callback_uses_the_configured_portal_prefix_only_on_its_origin() {
    let Some(cp) = TestCp::start_with(|cfg| {
        cfg.portal_url = Some("https://portal.example.com/control/tenant/".to_string());
    })
    .await
    else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let initial = cp.send("GET", SETTINGS, Some(admin.cookie()), None).await;
    assert_eq!(initial.status, 200, "default settings: {initial:?}");
    assert_eq!(initial.body["portal_origin"], "https://portal.example.com");
    assert_eq!(
        initial.body["github"]["callback_url"],
        "https://portal.example.com/control/tenant/api/auth/oauth/github/callback"
    );

    let mut moved = settings();
    moved["portal_origin"] = "https://new-portal.example.com".into();
    let updated = cp
        .send("PUT", SETTINGS, Some(admin.cookie()), Some(moved))
        .await;
    assert_eq!(
        updated.status, 200,
        "save settings on another origin: {updated:?}"
    );
    assert_eq!(
        updated.body["github"]["callback_url"],
        "https://new-portal.example.com/api/auth/oauth/github/callback"
    );
}

#[tokio::test]
async fn invalid_optional_provider_secret_does_not_block_password_login() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let member = cp.seed_member(tenant, MemberRole::Owner).await;
    let configured = cp
        .send("PUT", SETTINGS, Some(admin.cookie()), Some(settings()))
        .await;
    assert_eq!(configured.status, 200, "activate GitHub: {configured:?}");
    assert_eq!(configured.body["github"]["enabled"], true);

    let corrupted = sqlx::query(
        "UPDATE cp_settings SET value = jsonb_set(value, '{config,github,client_secret}', \
             to_jsonb('corrupt-ciphertext'::text), false) \
         WHERE key = 'authentication' AND value #> '{config,github,client_secret}' IS NOT NULL",
    )
    .execute(cp.db().pool())
    .await
    .expect("corrupt only the GitHub secret");
    assert_eq!(corrupted.rows_affected(), 1);
    let login = cp.member_login(&member.email, TEST_PASSWORD).await;
    assert_eq!(login.status, 200, "password login: {login:?}");
    assert_eq!(login.body["member"]["id"], member.id.to_string());
    assert!(
        login.set_cookie.is_some(),
        "password login must start a session"
    );
}
