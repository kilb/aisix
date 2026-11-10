//! Operator accounts: login, sessions, the last-admin rule, password
//! changes, the audit log and the `create-operator` command.

mod support;

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_control_plane::auth::OperatorRole;
use serde_json::{json, Value};
use sqlx::{Postgres, Transaction};
use support::{OperatorSession, TempFile, TestCp, TEST_PASSWORD};
use tokio::io::AsyncWriteExt;
use tokio::sync::Barrier;
use uuid::Uuid;

fn fresh_email(tag: &str) -> String {
    format!("{tag}-{}@ops.test", Uuid::new_v4().simple())
}

/// Every operator, as `GET /admin/operators` lists them.
async fn operators(cp: &TestCp, admin: &OperatorSession) -> Vec<Value> {
    let r = cp.get_with_cookie("/admin/operators", admin.cookie()).await;
    assert_eq!(r.status, 200, "{r:?}");
    r.body["items"].as_array().expect("items").clone()
}

fn find(items: &[Value], id: Uuid) -> &Value {
    items
        .iter()
        .find(|o| o["id"] == id.to_string())
        .unwrap_or_else(|| panic!("operator {id} is not listed: {items:?}"))
}

async fn patch(cp: &TestCp, by: &OperatorSession, id: Uuid, body: Value) -> support::Resp {
    cp.send(
        "PATCH",
        &format!("/admin/operators/{id}"),
        Some(by.cookie()),
        Some(body),
    )
    .await
}

/// Every audit row and every session row, as text.
async fn dump_audit_and_sessions(cp: &TestCp) -> String {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT row_to_json(a)::text FROM audit_log a \
         UNION ALL SELECT row_to_json(s)::text FROM sessions s",
    )
    .fetch_all(cp.db().pool())
    .await
    .expect("dump");
    rows.join("\n")
}

#[tokio::test]
async fn wrong_password_unknown_email_and_disabled_account_answer_alike() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let live = cp.seed_operator(OperatorRole::Admin).await;
    let blocked_email = fresh_email("blocked");
    cp.seed_operator_as(&blocked_email, OperatorRole::Admin, true)
        .await;

    let mut answers = Vec::new();
    for (email, password) in [
        (live.email.as_str(), "not-the-password-1"),
        ("nobody-7f3a@ops.test", TEST_PASSWORD),
        (blocked_email.as_str(), TEST_PASSWORD),
    ] {
        let before = cp.verification_count();
        let r = cp.admin_login(email, password).await;
        // The password is checked in all three cases, so none answers
        // faster than the others.
        assert_eq!(cp.verification_count(), before + 1, "{email}");
        assert_eq!(r.status, 401, "{email}: {r:?}");
        assert_eq!(r.body["code"], "invalid_credentials", "{email}");
        assert!(r.set_cookie.is_none(), "{email}: {r:?}");
        answers.push(r.body);
    }
    assert_eq!(answers[0], answers[1]);
    assert_eq!(answers[1], answers[2]);

    // The same password logs the live account in, so the disabled account
    // was refused for being disabled.
    let r = cp.admin_login(&live.email, TEST_PASSWORD).await;
    assert_eq!(r.status, 200, "{r:?}");

    // All three failures are audited, but the text a caller chose for an
    // email no account has is not stored.
    let failed: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT actor_email, target_id FROM audit_log \
         WHERE action = 'operator.login_failed' ORDER BY id",
    )
    .fetch_all(cp.db().pool())
    .await
    .expect("the failed logins");
    assert_eq!(failed.len(), 3, "{failed:?}");
    assert_eq!(failed[0].0.as_deref(), Some(live.email.as_str()));
    assert_eq!(failed[1], (None, None));
    assert_eq!(failed[2].0.as_deref(), Some(blocked_email.as_str()));
    assert!(
        !dump_audit_and_sessions(&cp).await.contains("nobody-7f3a"),
        "the unknown email was stored"
    );
}

#[tokio::test]
async fn five_failures_put_an_email_into_cooldown() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let target = cp.seed_operator(OperatorRole::Viewer).await;
    let bystander = cp.seed_operator(OperatorRole::Viewer).await;
    for attempt in 1..=5 {
        let r = cp.admin_login(&target.email, "not-the-password-1").await;
        assert_eq!(r.status, 401, "attempt {attempt}: {r:?}");
    }

    // Now even the right password is refused, before any argon2 work, and
    // however the email is spelled.
    let before = cp.verification_count();
    for spelling in [
        target.email.clone(),
        format!("  {}  ", target.email.to_uppercase()),
    ] {
        let r = cp.admin_login(&spelling, TEST_PASSWORD).await;
        assert_eq!(r.status, 429, "{spelling}: {r:?}");
        assert_eq!(r.body["code"], "busy");
        assert_eq!(
            r.headers.get("retry-after").and_then(|v| v.to_str().ok()),
            Some("60"),
            "{r:?}"
        );
        assert!(r.set_cookie.is_none());
    }
    assert_eq!(cp.verification_count(), before);

    // Another email is not affected.
    let r = cp.admin_login(&bystander.email, TEST_PASSWORD).await;
    assert_eq!(r.status, 200, "{r:?}");
}

#[tokio::test]
async fn the_admin_cookie_is_httponly_secure_and_samesite_strict() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.seed_operator(OperatorRole::Operator).await;
    let r = cp.admin_login(&op.email, TEST_PASSWORD).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["email"], op.email.as_str());
    assert_eq!(r.body["role"], "operator");
    let set_cookie = r.set_cookie.expect("a Set-Cookie header");
    let token = set_cookie
        .strip_prefix("aisix_cp_admin=")
        .and_then(|rest| rest.split(';').next())
        .expect("the admin cookie")
        .to_string();
    assert_eq!(token.len(), 64, "{set_cookie}");
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()), "{set_cookie}");
    assert_eq!(
        set_cookie,
        format!("aisix_cp_admin={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=28800")
    );

    // Only the SHA-256 of the token is stored.
    let (hashed, plain): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE token_sha256 = sha256(convert_to($1, 'UTF8'))), \
                count(*) FILTER (WHERE encode(token_sha256, 'hex') = $1) \
         FROM sessions",
    )
    .bind(&token)
    .fetch_one(cp.db().pool())
    .await
    .expect("look the session up");
    assert_eq!((hashed, plain), (1, 0));

    let cookie = format!("aisix_cp_admin={token}");
    let r = cp.get_with_cookie("/admin/session", &cookie).await;
    assert_eq!(r.body["authed"], true, "{r:?}");
    assert_eq!(r.body["api_contract"], 1);
    assert_eq!(r.body["operator"]["id"], op.id.to_string());

    // The admin API reads only its own cookie.
    let r = cp
        .get_with_cookie("/admin/session", &format!("aisix_cp_portal={token}"))
        .await;
    assert_eq!(r.body["authed"], false, "{r:?}");
    let r = cp
        .get_with_cookie("/admin/audit", &format!("aisix_cp_portal={token}"))
        .await;
    assert_eq!(r.status, 401, "{r:?}");

    // Logging out clears the cookie and ends the session.
    let r = cp.send("POST", "/admin/logout", Some(&cookie), None).await;
    assert_eq!(r.status, 204, "{r:?}");
    assert_eq!(
        r.set_cookie.as_deref(),
        Some("aisix_cp_admin=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0")
    );
    let r = cp.get_with_cookie("/admin/session", &cookie).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body, json!({"authed": false, "api_contract": 1}));
}

#[tokio::test]
async fn a_session_issued_by_one_instance_is_honoured_by_another() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let other = cp.restart().await;
    let r = other
        .get_with_cookie("/admin/session", admin.cookie())
        .await;
    assert_eq!(r.body["authed"], true, "{r:?}");
    assert_eq!(
        r.body["operator"],
        json!({"id": admin.id.to_string(), "email": admin.email, "display_name": null, "role": "admin"})
    );
}

#[tokio::test]
async fn disabling_an_operator_ends_their_live_sessions_at_once() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let victim = cp.admin_login_seeded(OperatorRole::Operator).await;
    let r = cp.get_with_cookie("/admin/audit", victim.cookie()).await;
    assert_eq!(r.status, 200, "{r:?}");

    let r = patch(&cp, &admin, victim.id, json!({"disabled": true})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = cp.get_with_cookie("/admin/audit", victim.cookie()).await;
    assert_eq!(r.status, 401, "{r:?}");
    assert_eq!(r.body["code"], "unauthenticated");
    let r = cp.get_with_cookie("/admin/session", victim.cookie()).await;
    assert_eq!(r.body["authed"], false, "{r:?}");
    assert!(find(&operators(&cp, &admin).await, victim.id)["disabled"] == true);

    // The sessions are gone, not merely refused: re-enabling the account
    // does not bring them back.
    let r = patch(&cp, &admin, victim.id, json!({"disabled": false})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = cp.get_with_cookie("/admin/audit", victim.cookie()).await;
    assert_eq!(r.status, 401, "{r:?}");
    let r = cp.admin_login(&victim.email, TEST_PASSWORD).await;
    assert_eq!(r.status, 200, "{r:?}");
}

#[tokio::test]
async fn a_disabled_accounts_session_is_refused_even_when_it_was_not_ended() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    // A login that raced the disable can leave a session behind; the
    // account's state is checked on every request.
    let op = cp.admin_login_seeded(OperatorRole::Admin).await;
    sqlx::query("UPDATE operators SET disabled = true WHERE id = $1")
        .bind(op.id)
        .execute(cp.db().pool())
        .await
        .expect("disable behind the API's back");
    let r = cp.get_with_cookie("/admin/audit", op.cookie()).await;
    assert_eq!(r.status, 401, "{r:?}");
    let r = cp.get_with_cookie("/admin/session", op.cookie()).await;
    assert_eq!(r.body["authed"], false, "{r:?}");
}

#[tokio::test]
async fn an_expired_session_is_refused() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let r = cp.get_with_cookie("/admin/audit", op.cookie()).await;
    assert_eq!(r.status, 200, "{r:?}");
    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second'")
        .execute(cp.db().pool())
        .await
        .expect("expire the session");
    let r = cp.get_with_cookie("/admin/audit", op.cookie()).await;
    assert_eq!(r.status, 401, "{r:?}");
}

#[tokio::test]
async fn the_last_admin_cannot_be_demoted_or_disabled() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    // A disabled admin is not a usable one.
    cp.seed_operator_as(&fresh_email("dormant"), OperatorRole::Admin, true)
        .await;
    for body in [
        json!({"role": "operator"}),
        json!({"role": "viewer"}),
        json!({"disabled": true}),
        json!({"role": "admin", "disabled": true}),
    ] {
        let r = patch(&cp, &admin, admin.id, body.clone()).await;
        assert_eq!(r.status, 409, "{body}: {r:?}");
        assert_eq!(r.body["code"], "last_admin", "{body}");
    }
    let me = find(&operators(&cp, &admin).await, admin.id).clone();
    assert_eq!(
        (me["role"].clone(), me["disabled"].clone()),
        (json!("admin"), json!(false))
    );
    // Changes that keep it an enabled admin go through.
    let r = patch(&cp, &admin, admin.id, json!({"display_name": "Root"})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["display_name"], "Root");

    // With a second enabled admin, the first can step down; then the
    // second is the last one.
    let second = cp.admin_login_seeded(OperatorRole::Admin).await;
    let r = patch(&cp, &second, admin.id, json!({"role": "operator"})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["role"], "operator");
    let r = patch(&cp, &second, second.id, json!({"disabled": true})).await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["code"], "last_admin");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_demotions_of_the_last_two_admins_leave_one_admin() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let cp = Arc::new(cp);
    let a = cp.admin_login_seeded(OperatorRole::Admin).await;
    let b = cp.admin_login_seeded(OperatorRole::Admin).await;
    for round in 0..20 {
        sqlx::query("UPDATE operators SET role = 'admin', disabled = false WHERE id = ANY($1)")
            .bind(vec![a.id, b.id])
            .execute(cp.db().pool())
            .await
            .expect("reset both to admin");
        // Each steps down at the same moment: each alone would leave the
        // other as an admin, both together would leave none.
        let barrier = Arc::new(Barrier::new(2));
        let tasks: Vec<_> = [a.clone(), b.clone()]
            .into_iter()
            .map(|me| {
                let cp = cp.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    patch(&cp, &me, me.id, json!({"role": "operator"})).await
                })
            })
            .collect();
        let mut statuses = Vec::new();
        for task in tasks {
            let r = task.await.expect("join");
            if r.status == 409 {
                assert_eq!(r.body["code"], "last_admin", "round {round}");
            }
            statuses.push(r.status);
        }
        statuses.sort_unstable();
        assert_eq!(statuses, vec![200, 409], "round {round}");
        let admins: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM operators WHERE role = 'admin' AND NOT disabled",
        )
        .fetch_one(cp.db().pool())
        .await
        .expect("count admins");
        assert_eq!(admins, 1, "round {round}");
    }
}

#[tokio::test]
async fn creating_an_operator_is_audited_without_the_password_or_its_hash() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let password = "pw-audit-7c1-long-enough";
    let email = fresh_email("new");
    let r = cp
        .send(
            "POST",
            "/admin/operators",
            Some(admin.cookie()),
            Some(json!({
                "email": format!("  {}  ", email.to_uppercase()),
                "display_name": "New Viewer",
                "role": "viewer",
                "password": password,
            })),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let id: Uuid = r.body["id"].as_str().expect("id").parse().expect("a UUID");
    assert_eq!(
        r.body,
        json!({"id": id.to_string(), "email": email, "display_name": "New Viewer", "role": "viewer"})
    );
    let stored_hash: String =
        sqlx::query_scalar("SELECT password_hash FROM operators WHERE id = $1")
            .bind(id)
            .fetch_one(cp.db().pool())
            .await
            .expect("the stored hash");
    assert!(stored_hash.starts_with("$argon2"), "{stored_hash}");

    // The email is taken now, however it is spelled; and invalid fields are
    // named, all at once.
    for (body, status, code) in [
        (
            json!({"email": email.to_uppercase(), "role": "admin", "password": "another-pw-long"}),
            409,
            "email_taken",
        ),
        (
            json!({"email": "not-an-email", "role": "admin", "password": "short",
                   "display_name": "x".repeat(129)}),
            422,
            "validation_failed",
        ),
    ] {
        let r = cp
            .send("POST", "/admin/operators", Some(admin.cookie()), Some(body))
            .await;
        assert_eq!(
            (r.status, r.body["code"].clone()),
            (status, json!(code)),
            "{r:?}"
        );
        if status == 422 {
            let paths: Vec<&str> = r.body["details"]
                .as_array()
                .expect("details")
                .iter()
                .filter_map(|d| d["path"].as_str())
                .collect();
            assert_eq!(paths, ["/email", "/display_name", "/password"], "{r:?}");
        }
    }

    // The new account works, and so does a failed attempt with another
    // password: both land in the audit log too.
    let r = cp.admin_login(&email, password).await;
    assert_eq!(r.status, 200, "{r:?}");
    let failed_password = "pw-failed-9d2-long-enough";
    let r = cp.admin_login(&email, failed_password).await;
    assert_eq!(r.status, 401, "{r:?}");

    let r = cp
        .get_with_cookie(
            &format!("/admin/audit?target_kind=operator&target_id={id}"),
            admin.cookie(),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let items = r.body["items"].as_array().expect("items").clone();
    let actions: Vec<&str> = items
        .iter()
        .map(|i| i["action"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        actions,
        ["operator.login_failed", "operator.login", "operator.create"],
        "{items:?}"
    );
    let created = &items[2];
    assert_eq!(
        created["actor"],
        json!({"kind": "operator", "id": admin.id.to_string(), "email": admin.email})
    );
    assert_eq!(created["target_kind"], "operator");
    assert_eq!(created["target_id"], id.to_string());
    assert_eq!(created["detail"]["role"], "viewer");
    assert_eq!(created["detail"]["email"], email.as_str());
    assert_eq!(items[0]["actor"]["kind"], "anonymous");
    assert_eq!(items[0]["actor"]["email"], email.as_str());

    let everything = format!("{}\n{}", r.body, dump_audit_and_sessions(&cp).await);
    for secret in [
        password,
        failed_password,
        stored_hash.as_str(),
        TEST_PASSWORD,
    ] {
        assert!(
            !everything.contains(secret),
            "{secret} leaked: {everything}"
        );
    }
}

#[tokio::test]
async fn changing_ones_password_ends_ones_other_sessions() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let me = cp.seed_operator(OperatorRole::Viewer).await;
    let here = cp.admin_login_as(&me).await;
    let elsewhere = cp.admin_login_as(&me).await;
    let someone_else = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let new_password = "new-password-5e1-long";

    let change =
        |current: &str, new: &str| json!({"current_password": current, "new_password": new});
    let r = cp
        .send(
            "POST",
            "/admin/password",
            Some(here.cookie()),
            Some(change("not-the-password-1", new_password)),
        )
        .await;
    assert_eq!(r.status, 401, "{r:?}");
    assert_eq!(r.body["code"], "invalid_credentials");
    let r = cp
        .send(
            "POST",
            "/admin/password",
            Some(here.cookie()),
            Some(change(TEST_PASSWORD, "too-short")),
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/new_password");
    // Neither attempt changed anything.
    let r = cp
        .get_with_cookie("/admin/session", elsewhere.cookie())
        .await;
    assert_eq!(r.body["authed"], true, "{r:?}");

    let r = cp
        .send(
            "POST",
            "/admin/password",
            Some(here.cookie()),
            Some(change(TEST_PASSWORD, new_password)),
        )
        .await;
    assert_eq!(r.status, 204, "{r:?}");
    for (session, authed) in [(&here, true), (&elsewhere, false), (&someone_else, true)] {
        let r = cp.get_with_cookie("/admin/session", session.cookie()).await;
        assert_eq!(r.body["authed"], authed, "{}: {r:?}", session.email);
    }
    assert_eq!(cp.admin_login(&me.email, TEST_PASSWORD).await.status, 401);
    assert_eq!(cp.admin_login(&me.email, new_password).await.status, 200);

    let r = cp
        .get_with_cookie(
            &format!("/admin/audit?target_kind=operator&target_id={}", me.id),
            someone_else.cookie(),
        )
        .await;
    assert!(
        r.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .any(|i| i["action"] == "operator.password" && i["actor"]["id"] == me.id.to_string()),
        "{r:?}"
    );
    let everything = dump_audit_and_sessions(&cp).await;
    for secret in [new_password, TEST_PASSWORD, "not-the-password-1"] {
        assert!(!everything.contains(secret), "{secret} leaked");
    }
}

#[tokio::test]
async fn the_audit_log_pages_newest_first() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    for name in ["one", "two", "three"] {
        let r = patch(&cp, &admin, viewer.id, json!({"display_name": name})).await;
        assert_eq!(r.status, 200, "{r:?}");
    }
    let path = format!(
        "/admin/audit?target_kind=operator&target_id={}&limit=2",
        viewer.id
    );
    // The viewer's own login, then the three updates.
    let first = cp.get_with_cookie(&path, viewer.cookie()).await;
    assert_eq!(first.status, 200, "{first:?}");
    let names = |page: &Value| -> Vec<Value> {
        page["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|i| i["detail"]["display_name"].clone())
            .collect()
    };
    assert_eq!(names(&first.body), [json!("three"), json!("two")]);
    let before = first.body["next_before"].as_i64().expect("a next page");
    let second = cp
        .get_with_cookie(&format!("{path}&before={before}"), viewer.cookie())
        .await;
    assert_eq!(names(&second.body), [json!("one"), Value::Null]);
    assert_eq!(second.body["items"][1]["action"], "operator.login");
    // The second page reaches the end.
    assert_eq!(second.body["next_before"], Value::Null, "{second:?}");
    let oldest = second.body["items"][1]["id"].as_i64().expect("an id");
    let past_the_end = cp
        .get_with_cookie(&format!("{path}&before={oldest}"), viewer.cookie())
        .await;
    assert_eq!(past_the_end.body, json!({"items": [], "next_before": null}));

    let r = cp
        .get_with_cookie("/admin/audit?limit=many", viewer.cookie())
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["code"], "validation_failed");
}

/// Runs `create-operator` with `args`, `stdin` as its input and the
/// configuration of `cp`.
async fn create_operator(cp: &TestCp, args: &[&str], stdin: &str) -> (i32, String) {
    let key = TempFile::with_contents(hex::encode(support::TEST_MASTER_KEY).as_bytes());
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_aisix-control-plane"))
        .arg("create-operator")
        .args(args)
        .env_clear()
        .env("CP_DATABASE_URL", cp.database_url())
        .env("CP_MASTER_KEY_FILE", key.path())
        .env("CP_DP_HOSTS", "localhost")
        .env("RUST_LOG", "warn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the control-plane binary");
    let mut input = child.stdin.take().expect("stdin");
    input
        .write_all(stdin.as_bytes())
        .await
        .expect("write stdin");
    drop(input);
    let out = child.wait_with_output().await.expect("wait");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

#[tokio::test]
async fn the_create_operator_command_makes_the_first_admin() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let email = fresh_email("first");
    let password = "first-admin-pw-3c9";
    let (code, out) = create_operator(
        &cp,
        &[
            "--email",
            &email,
            "--role",
            "admin",
            "--display-name",
            "First",
        ],
        &format!("{password}\nthe second line is ignored\n"),
    )
    .await;
    assert_eq!(code, 0, "{out}");
    assert!(!out.contains(password), "{out}");

    let r = cp.admin_login(&email, password).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["role"], "admin");
    assert_eq!(r.body["display_name"], "First");
    let created: (String, Option<Uuid>, Value) = sqlx::query_as(
        "SELECT actor_kind, actor_id, detail FROM audit_log WHERE action = 'operator.create'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("the audit row");
    assert_eq!((created.0.as_str(), created.1), ("system", None));
    assert_eq!(created.2["role"], "admin");

    // The same email again is refused.
    let (code, out) = create_operator(
        &cp,
        &["--email", &email, "--role", "viewer"],
        "another-password-1\n",
    )
    .await;
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("already exists"), "{out}");
    let (code, out) = create_operator(
        &cp,
        &["--email", &fresh_email("short"), "--role", "viewer"],
        "short-pw\n",
    )
    .await;
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("12"), "{out}");
    let (code, out) = create_operator(
        &cp,
        &["--email", &fresh_email("norole")],
        "long-enough-password\n",
    )
    .await;
    assert_eq!(code, 2, "{out}");
    let (code, out) = create_operator(
        &cp,
        &["--email", &fresh_email("badrole"), "--role", "root"],
        "long-enough-password\n",
    )
    .await;
    assert_eq!(code, 2, "{out}");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM operators")
        .fetch_one(cp.db().pool())
        .await
        .expect("count");
    assert_eq!(count, 1);
}

/// Backends of `cp`'s database that are waiting for a lock.
async fn lock_waiters(cp: &TestCp) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity \
         WHERE datname = current_database() AND wait_event_type = 'Lock'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("read pg_stat_activity")
}

/// Returns once `n` backends wait for a lock. It polls a condition the test
/// itself sets up, so it does not depend on timing.
async fn until_lock_waiters(cp: &TestCp, n: i64) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while lock_waiters(cp).await < n {
        assert!(Instant::now() < deadline, "{n} lock waiters never appeared");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A transaction that holds back every insert into `audit_log` until it
/// ends: a change blocks on its audit row after doing everything else.
async fn hold_audit_inserts(cp: &TestCp) -> Transaction<'static, Postgres> {
    let mut holder = cp.db().pool().begin().await.expect("begin");
    sqlx::query("LOCK TABLE audit_log IN SHARE MODE")
        .execute(&mut *holder)
        .await
        .expect("hold audit_log");
    holder
}

async fn live_sessions_of(cp: &TestCp, id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM sessions WHERE principal_id = $1 AND expires_at > now()",
    )
    .bind(id)
    .fetch_one(cp.db().pool())
    .await
    .expect("count sessions")
}

/// The session `login` opened, if any, as a `Cookie` header value.
fn cookie_of(login: &support::Resp) -> Option<String> {
    login
        .set_cookie
        .as_deref()
        .and_then(|c| c.split(';').next())
        .map(str::to_string)
}

/// Lock waits here last as long as the test needs, however slow argon2 is.
fn patient(cfg: &mut aisix_control_plane::config::CpConfig) {
    cfg.db_lock_timeout_ms = 120_000;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_login_racing_a_password_change_gets_no_lasting_session() {
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let cp = Arc::new(cp);
    let me = cp.seed_operator(OperatorRole::Viewer).await;
    let changer = cp.admin_login_as(&me).await;
    let holder = hold_audit_inserts(&cp).await;

    // The change replaces the hash and ends the other sessions, then waits
    // to write its audit row, uncommitted.
    let change = tokio::spawn({
        let (cp, cookie) = (cp.clone(), changer.cookie().to_string());
        async move {
            cp.send(
                "POST",
                "/admin/password",
                Some(&cookie),
                Some(json!({"current_password": TEST_PASSWORD, "new_password": "raced-new-password-1"})),
            )
            .await
        }
    });
    until_lock_waiters(&cp, 1).await;
    // Meanwhile a login with the old password still sees the old hash.
    let login = tokio::spawn({
        let (cp, email) = (cp.clone(), me.email.clone());
        async move { cp.admin_login(&email, TEST_PASSWORD).await }
    });
    until_lock_waiters(&cp, 2).await;
    holder.rollback().await.expect("release audit_log");

    let change = change.await.expect("join");
    assert_eq!(change.status, 204, "{change:?}");
    let login = login.await.expect("join");
    if let Some(cookie) = cookie_of(&login) {
        let r = cp.get_with_cookie("/admin/session", &cookie).await;
        assert_eq!(
            r.body["authed"], false,
            "the raced login kept a session: {r:?}"
        );
    }
    assert_eq!(login.status, 401, "{login:?}");
    // Only the changer's own session is left.
    assert_eq!(live_sessions_of(&cp, me.id).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_login_racing_a_disable_gets_no_session_back_on_re_enable() {
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let cp = Arc::new(cp);
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let victim = cp.seed_operator(OperatorRole::Operator).await;
    let holder = hold_audit_inserts(&cp).await;

    let disable = tokio::spawn({
        let (cp, admin, id) = (cp.clone(), admin.clone(), victim.id);
        async move { patch(&cp, &admin, id, json!({"disabled": true})).await }
    });
    until_lock_waiters(&cp, 1).await;
    let login = tokio::spawn({
        let (cp, email) = (cp.clone(), victim.email.clone());
        async move { cp.admin_login(&email, TEST_PASSWORD).await }
    });
    until_lock_waiters(&cp, 2).await;
    holder.rollback().await.expect("release audit_log");

    let disable = disable.await.expect("join");
    assert_eq!(disable.status, 200, "{disable:?}");
    let login = login.await.expect("join");
    assert_eq!(live_sessions_of(&cp, victim.id).await, 0);
    // A session that slipped past the disable would come back with the
    // account.
    let r = patch(&cp, &admin, victim.id, json!({"disabled": false})).await;
    assert_eq!(r.status, 200, "{r:?}");
    if let Some(cookie) = cookie_of(&login) {
        let r = cp.get_with_cookie("/admin/session", &cookie).await;
        assert_eq!(
            r.body["authed"], false,
            "the raced login kept a session: {r:?}"
        );
    }
    assert_eq!(login.status, 401, "{login:?}");
}

/// What is sent, its `Content-Type`, the body, and the expected status and
/// code.
type BodyCase = (
    &'static str,
    Option<&'static str>,
    Vec<u8>,
    u16,
    &'static str,
);

#[tokio::test]
async fn a_body_the_api_cannot_read_answers_in_the_error_envelope() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let json_type = Some("application/json");
    let too_large = serde_json::to_vec(&json!({
        "email": fresh_email("big"),
        "role": "viewer",
        "password": "x".repeat((1 << 20) + 1024),
    }))
    .expect("serialize");
    let cases: Vec<BodyCase> = vec![
        (
            "not JSON",
            json_type,
            b"{\"email\":".to_vec(),
            422,
            "validation_failed",
        ),
        (
            "no content type",
            None,
            b"{}".to_vec(),
            415,
            "unsupported_media_type",
        ),
        (
            "a missing field",
            json_type,
            br#"{"email":"a@b.io"}"#.to_vec(),
            422,
            "validation_failed",
        ),
        (
            "an unknown role",
            json_type,
            br#"{"email":"a@b.io","role":"root-7c1","password":"long-enough-pw"}"#.to_vec(),
            422,
            "validation_failed",
        ),
        ("over 1 MiB", json_type, too_large, 413, "payload_too_large"),
    ];
    for (what, content_type, body, status, code) in cases {
        let r = cp
            .send_raw(
                "POST",
                "/admin/operators",
                Some(admin.cookie()),
                content_type,
                Some(body),
            )
            .await;
        assert_eq!(r.status, status, "{what}: {r:?}");
        assert_eq!(r.body["code"], code, "{what}: {r:?}");
        assert!(
            r.body["error"].as_str().is_some_and(|e| !e.is_empty()),
            "{what}: {r:?}"
        );
        assert!(
            !r.body.to_string().contains("root-7c1"),
            "{what} echoed: {r:?}"
        );
    }

    // A login body that cannot be read is a failed login like any other.
    for body in [b"{\"email\":".to_vec(), br#"{"email":"a@b.io"}"#.to_vec()] {
        let r = cp
            .send_raw("POST", "/admin/login", None, json_type, Some(body))
            .await;
        assert_eq!(r.status, 401, "{r:?}");
        assert_eq!(r.body["code"], "invalid_credentials", "{r:?}");
    }
    // One that is not JSON at all is refused before the login is tried, as
    // on every mutating route.
    let r = cp
        .send_raw(
            "POST",
            "/admin/login",
            None,
            None,
            Some(br#"{"email":"a@b.io","password":"x"}"#.to_vec()),
        )
        .await;
    assert_eq!(r.status, 415, "{r:?}");
    assert_eq!(r.body["code"], "unsupported_media_type", "{r:?}");
}

/// Every mutating request on the browser listener must say it is JSON,
/// bodyless ones included, or it is `415 unsupported_media_type` and
/// changes nothing. With `SameSite=Strict` only a same-site page can still
/// make a browser send a session cookie, and it cannot send this content
/// type without a CORS preflight it does not pass: a form posts another
/// type, and a `no-cors` fetch with no body sends none.
#[tokio::test]
async fn a_mutating_request_that_is_not_json_is_415_and_changes_nothing() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp
        .seed_tenant(aisix_control_plane::tenants::TenantStatus::Pending)
        .await;
    let approve = format!("/admin/tenants/{tenant}/approve");
    let status = || async {
        sqlx::query_scalar::<_, String>("SELECT status FROM tenants WHERE id = $1")
            .bind(tenant)
            .fetch_one(cp.db().pool())
            .await
            .expect("read the tenant")
    };
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
        Some("multipart/form-data; boundary=x"),
        Some("application/jsonx"),
    ] {
        let r = cp
            .send_raw("POST", &approve, Some(op.cookie()), content_type, None)
            .await;
        assert_eq!(r.status, 415, "{content_type:?}: {r:?}");
        assert_eq!(r.body["code"], "unsupported_media_type", "{r:?}");
        assert_eq!(status().await, "pending", "{content_type:?} approved it");
    }
    // A body does not help without the type.
    let r = cp
        .send_raw(
            "POST",
            &approve,
            Some(op.cookie()),
            Some("text/plain"),
            Some(b"{}".to_vec()),
        )
        .await;
    assert_eq!(r.status, 415, "{r:?}");
    assert_eq!(status().await, "pending");

    // Logout, on both APIs, is held to it too: the session survives.
    for path in ["/admin/logout", "/api/logout"] {
        let r = cp
            .send_raw("POST", path, Some(op.cookie()), None, None)
            .await;
        assert_eq!(r.status, 415, "{path}: {r:?}");
    }
    let session = cp.get_with_cookie("/admin/session", op.cookie()).await;
    assert_eq!(session.body["authed"], true, "{session:?}");
    // So is a DELETE.
    let r = cp
        .send_raw(
            "DELETE",
            &format!("/admin/resources/models/{}", Uuid::new_v4()),
            Some(op.cookie()),
            None,
            None,
        )
        .await;
    assert_eq!(r.status, 415, "{r:?}");

    // Reads need no type, and a route nobody serves is still not found.
    let r = cp
        .send_raw("GET", "/admin/session", Some(op.cookie()), None, None)
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = cp
        .send_raw("POST", "/admin/nothing-7c1", Some(op.cookie()), None, None)
        .await;
    assert_eq!(r.status, 404, "{r:?}");

    // JSON with a charset, and no body, is what the Dashboard sends.
    let r = cp
        .send_raw(
            "POST",
            &approve,
            Some(op.cookie()),
            Some("Application/JSON; charset=utf-8"),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(status().await, "active");
}

/// Whether operator `id` is disabled and their role, as stored.
async fn stored_operator(cp: &TestCp, id: Uuid) -> (String, bool) {
    sqlx::query_as("SELECT role, disabled FROM operators WHERE id = $1")
        .bind(id)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the operator")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_change_of_a_disabled_admin_cannot_undo_the_disable() {
    // B's request was authorised while B was still an enabled admin, and
    // then queued behind A's disable of B. Once the disable commits, the
    // queued change must not turn B back on.
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let cp = Arc::new(cp);
    let a = cp.admin_login_seeded(OperatorRole::Admin).await;
    let b = cp.admin_login_seeded(OperatorRole::Admin).await;
    let holder = hold_audit_inserts(&cp).await;

    let disable = tokio::spawn({
        let (cp, a, id) = (cp.clone(), a.clone(), b.id);
        async move { patch(&cp, &a, id, json!({"disabled": true})).await }
    });
    until_lock_waiters(&cp, 1).await;
    let undo = tokio::spawn({
        let (cp, b) = (cp.clone(), b.clone());
        async move { patch(&cp, &b, b.id, json!({"disabled": false, "role": "admin"})).await }
    });
    until_lock_waiters(&cp, 2).await;
    holder.rollback().await.expect("release audit_log");

    let disable = disable.await.expect("join");
    assert_eq!(disable.status, 200, "{disable:?}");
    let undo = undo.await.expect("join");
    assert_eq!(undo.status, 401, "{undo:?}");
    assert_eq!(
        stored_operator(&cp, b.id).await,
        ("admin".to_string(), true)
    );
    assert_eq!(live_sessions_of(&cp, b.id).await, 0);
    assert_eq!(cp.admin_login(&b.email, TEST_PASSWORD).await.status, 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_change_of_a_demoted_admin_is_refused() {
    // B, demoted while its requests waited, can neither create an admin nor
    // promote another operator with the authority it no longer has.
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let cp = Arc::new(cp);
    let a = cp.admin_login_seeded(OperatorRole::Admin).await;
    let b = cp.admin_login_seeded(OperatorRole::Admin).await;
    let c = cp.seed_operator(OperatorRole::Viewer).await;
    let holder = hold_audit_inserts(&cp).await;

    let demote = tokio::spawn({
        let (cp, a, id) = (cp.clone(), a.clone(), b.id);
        async move { patch(&cp, &a, id, json!({"role": "viewer"})).await }
    });
    until_lock_waiters(&cp, 1).await;
    let email = fresh_email("queued-create-7c1");
    let create = tokio::spawn({
        let (cp, b, email) = (cp.clone(), b.clone(), email.clone());
        async move {
            cp.send(
                "POST",
                "/admin/operators",
                Some(b.cookie()),
                Some(json!({
                    "email": email,
                    "role": "admin",
                    "password": "queued-create-password-1",
                })),
            )
            .await
        }
    });
    until_lock_waiters(&cp, 2).await;
    let promote = tokio::spawn({
        let (cp, b, id) = (cp.clone(), b.clone(), c.id);
        async move { patch(&cp, &b, id, json!({"role": "admin"})).await }
    });
    until_lock_waiters(&cp, 3).await;
    holder.rollback().await.expect("release audit_log");

    let demote = demote.await.expect("join");
    assert_eq!(demote.status, 200, "{demote:?}");
    let create = create.await.expect("join");
    assert_eq!(create.status, 403, "{create:?}");
    let promote = promote.await.expect("join");
    assert_eq!(promote.status, 403, "{promote:?}");
    let created: i64 = sqlx::query_scalar("SELECT count(*) FROM operators WHERE email = $1")
        .bind(&email)
        .fetch_one(cp.db().pool())
        .await
        .expect("count operators");
    assert_eq!(created, 0);
    assert_eq!(
        stored_operator(&cp, c.id).await,
        ("viewer".to_string(), false)
    );
    assert_eq!(
        stored_operator(&cp, b.id).await,
        ("viewer".to_string(), false)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn of_two_password_changes_from_one_password_the_second_is_refused() {
    // Both changes verify the same current password before either writes.
    // The first commits; the second's current password is no longer the
    // account's, so it must neither replace the new password nor end the
    // session the first change kept.
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let cp = Arc::new(cp);
    let me = cp.seed_operator(OperatorRole::Viewer).await;
    let first_session = cp.admin_login_as(&me).await;
    let second_session = cp.admin_login_as(&me).await;
    let holder = hold_audit_inserts(&cp).await;

    let change = |session: &OperatorSession, new: &'static str| {
        let (cp, cookie) = (cp.clone(), session.cookie().to_string());
        tokio::spawn(async move {
            cp.send(
                "POST",
                "/admin/password",
                Some(&cookie),
                Some(json!({"current_password": TEST_PASSWORD, "new_password": new})),
            )
            .await
        })
    };
    let first = change(&first_session, "first-new-password-7c1");
    until_lock_waiters(&cp, 1).await;
    let second = change(&second_session, "second-new-password-7c1");
    until_lock_waiters(&cp, 2).await;
    holder.rollback().await.expect("release audit_log");

    let first = first.await.expect("join");
    assert_eq!(first.status, 204, "{first:?}");
    let second = second.await.expect("join");
    assert_eq!(second.status, 401, "{second:?}");
    assert_eq!(second.body["code"], "invalid_credentials", "{second:?}");
    assert_eq!(
        cp.admin_login(&me.email, "second-new-password-7c1")
            .await
            .status,
        401
    );
    let r = cp
        .get_with_cookie("/admin/session", first_session.cookie())
        .await;
    assert_eq!(r.body["authed"], true, "{r:?}");
    let r = cp
        .get_with_cookie("/admin/session", second_session.cookie())
        .await;
    assert_eq!(r.body["authed"], false, "{r:?}");
    assert_eq!(
        cp.admin_login(&me.email, "first-new-password-7c1")
            .await
            .status,
        200
    );
}
