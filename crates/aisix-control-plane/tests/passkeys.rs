mod support;

use aisix_control_plane::auth::OperatorRole;
use serde_json::json;
use support::{Resp, TestCp, TEST_PASSWORD};
use uuid::Uuid;

fn cookie(response: &Resp) -> &str {
    response
        .set_cookie
        .as_deref()
        .and_then(|value| value.split(';').next())
        .expect("a member session")
}

const REGISTER_START: &str = "/api/auth/passkeys/register/start";
const EMAIL: &str = "passkey-owner@example.net";

fn create(password: &str) -> Option<serde_json::Value> {
    Some(json!({"name": "Security key", "password": password}))
}

async fn enable_passkeys(cp: &TestCp) {
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let enabled = cp
        .send(
            "PUT",
            "/admin/settings/authentication",
            Some(admin.cookie()),
            Some(json!({
                "version": 0,
                "portal_origin": "https://portal.example.com",
                "email": {"enabled": false, "endpoint": "", "from": ""},
                "turnstile": {"enabled": false, "site_key": ""},
                "passkey": {"enabled": true},
                "github": {"enabled": false, "client_id": ""},
                "google": {"enabled": false, "client_id": ""}
            })),
        )
        .await;
    assert_eq!(enabled.status, 200, "{enabled:?}");
}

async fn signup(cp: &TestCp) -> Resp {
    let response = cp
        .send(
            "POST",
            "/api/signup",
            None,
            Some(json!({
                "org_name": "Passkey integration",
                "email": EMAIL,
                "password": TEST_PASSWORD
            })),
        )
        .await;
    assert_eq!(response.status, 201, "{response:?}");
    response
}

#[tokio::test]
async fn registration_requires_a_member_and_their_current_password() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    enable_passkeys(&cp).await;
    let anonymous = cp
        .send("POST", REGISTER_START, None, create(TEST_PASSWORD))
        .await;
    assert_eq!(anonymous.status, 401, "{anonymous:?}");
    let signup = signup(&cp).await;
    let cookie = cookie(&signup);
    let wrong = cp
        .send(
            "POST",
            REGISTER_START,
            Some(cookie),
            create("wrong password"),
        )
        .await;
    assert_eq!(wrong.status, 401, "{wrong:?}");
    let challenge = cp
        .send("POST", REGISTER_START, Some(cookie), create(TEST_PASSWORD))
        .await;
    assert_eq!(challenge.status, 200, "{challenge:?}");
    assert_eq!(
        challenge.body["options"]["publicKey"]["rp"]["id"],
        "portal.example.com"
    );
    assert_eq!(
        challenge.body["options"]["publicKey"]["authenticatorSelection"]["userVerification"],
        "required"
    );
    let listed = cp
        .send("GET", "/api/auth/passkeys", Some(cookie), None)
        .await;
    assert_eq!(listed.status, 200, "{listed:?}");
    assert_eq!(listed.body, json!({"passkeys": []}));
}

#[tokio::test]
async fn changing_the_portal_session_does_not_make_challenges_unbounded() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    enable_passkeys(&cp).await;
    signup(&cp).await;
    let mut issued = Vec::new();
    for _ in 0..19 {
        let session = cp
            .send(
                "POST",
                "/api/login",
                None,
                Some(json!({"email": EMAIL, "password": TEST_PASSWORD})),
            )
            .await;
        assert_eq!(session.status, 200, "{session:?}");
        let challenge = cp
            .send(
                "POST",
                REGISTER_START,
                Some(cookie(&session)),
                create(TEST_PASSWORD),
            )
            .await;
        assert_eq!(challenge.status, 200, "{challenge:?}");
        issued.push(
            serde_json::from_value::<Uuid>(challenge.body["challenge_id"].clone())
                .expect("a unique challenge ID"),
        );
    }
    let stored: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM portal_passkey_challenges")
        .fetch_all(cp.db().pool())
        .await
        .expect("read challenges");
    assert_eq!(stored.len(), 16);
    assert!(issued[..3].iter().all(|oldest| !stored.contains(oldest)));
    assert!(issued[3..].iter().all(|newest| stored.contains(newest)));
}
