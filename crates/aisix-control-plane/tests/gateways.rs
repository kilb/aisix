//! The control plane's CA, gateway credentials, and the deployment bundle a
//! gateway boots from.
//!
//! The bundle is checked against the gateway's own reading of it: the files
//! and SAN walk of `crates/aisix-server/src/cert_bundle.rs`, the mTLS client
//! of `crates/aisix-server/src/heartbeat.rs`, and the gateway's
//! configuration loader on the image's `config.managed.yaml`.

mod support;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::crypto::MasterKey;
use aisix_control_plane::db::Db;
use aisix_control_plane::pki::{self, Pki};
use rcgen::KeyPair;
use serde_json::{json, Value};
use support::{OperatorSession, Resp, TestCp, TEST_MASTER_KEY};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;
use x509_parser::extensions::GeneralName;
use x509_parser::pem::parse_x509_pem;
use x509_parser::prelude::{FromDer, X509Certificate};

/// Every variable the bundle carries, and nothing else.
const BUNDLE_VARS: [&str; 12] = [
    "AISIX_CONFIG_PATH",
    "AISIX_MANAGED__ENABLED",
    "AISIX_MANAGED__CP_BASE_URL",
    "AISIX_MANAGED__CP_ETCD_ENDPOINT",
    "AISIX_MANAGED__CP_CERT_PEM",
    "AISIX_MANAGED__CP_KEY_PEM",
    "AISIX_MANAGED__CP_CA_PEM",
    "AISIX_ETCD__PREFIX",
    "AISIX_MANAGED__SNAPSHOT_CACHE_ENABLED",
    "AISIX_PROXY__PLATFORM__ENABLED",
    "AISIX_PROXY__PLATFORM__ID_KEY_ENV",
    "PLATFORM_ID_KEY",
];

async fn issue(cp: &TestCp, by: &OperatorSession, body: Value) -> Resp {
    cp.send(
        "POST",
        "/admin/gateways/credentials",
        Some(by.cookie()),
        Some(body),
    )
    .await
}

async fn revoke(cp: &TestCp, by: &OperatorSession, dp_id: &str) -> Resp {
    cp.send(
        "POST",
        &format!("/admin/gateways/credentials/{dp_id}/revoke"),
        Some(by.cookie()),
        None,
    )
    .await
}

fn str_at<'a>(body: &'a Value, pointer: &str) -> &'a str {
    body.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{pointer} is a string in {body}"))
}

fn der_of(pem: &str) -> Vec<u8> {
    parse_x509_pem(pem.as_bytes())
        .expect("a PEM certificate")
        .1
        .contents
}

/// The certificate's SubjectPublicKeyInfo, DER.
fn spki_of(pem: &str) -> Vec<u8> {
    let der = der_of(pem);
    let (_, cert) = X509Certificate::from_der(&der).expect("an X.509 certificate");
    cert.public_key().raw.to_vec()
}

/// The `(env_id, dp_id)` a gateway reads from its leaf certificate: the walk
/// of `parse_san_uris` in `crates/aisix-server/src/cert_bundle.rs`, which is
/// private to the gateway binary.
fn gateway_san_uris(pem: &str) -> (String, String) {
    let (_, p) = parse_x509_pem(pem.as_bytes()).expect("decode PEM");
    let (_, cert) = X509Certificate::from_der(&p.contents).expect("parse X.509");
    let san = cert
        .subject_alternative_name()
        .expect("read the SAN extension")
        .expect("a SubjectAlternativeName extension");
    let mut env_id = String::new();
    let mut dp_id = String::new();
    for name in &san.value.general_names {
        if let GeneralName::URI(uri) = name {
            if let Some(rest) = uri.strip_prefix("x-aisix://env/") {
                env_id = rest.to_string();
            } else if let Some(rest) = uri.strip_prefix("x-aisix://dp/") {
                dp_id = rest.to_string();
            }
        }
    }
    (env_id, dp_id)
}

/// Every row of every table, as text.
async fn dump_all_tables(cp: &TestCp) -> String {
    let tables: Vec<String> =
        sqlx::query_scalar("SELECT tablename::text FROM pg_tables WHERE schemaname = 'public'")
            .fetch_all(cp.db().pool())
            .await
            .expect("list the tables");
    assert!(tables.iter().any(|t| t == "dp_credentials"), "{tables:?}");
    let mut dump = String::new();
    for table in tables {
        let rows: String = sqlx::query_scalar(&format!(
            "SELECT coalesce(string_agg(row_to_json(t)::text, E'\\n'), '') FROM \"{table}\" t"
        ))
        .fetch_one(cp.db().pool())
        .await
        .unwrap_or_else(|e| panic!("dump {table}: {e}"));
        dump.push_str(&rows);
        dump.push('\n');
    }
    dump
}

/// `(action, detail)` of every audit row, oldest first.
async fn audit_rows(cp: &TestCp) -> Vec<(String, String)> {
    sqlx::query_as("SELECT action, detail::text FROM audit_log ORDER BY id")
        .fetch_all(cp.db().pool())
        .await
        .expect("read the audit log")
}

async fn credential_count(cp: &TestCp) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM dp_credentials")
        .fetch_one(cp.db().pool())
        .await
        .expect("count credentials")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_instances_booting_together_agree_on_one_ca() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    // Two instances, each with its own pool, as two processes would have.
    let a = Db::connect(cp.database_url(), 5_000, 30_000)
        .await
        .expect("instance a connects");
    let b = Db::connect(cp.database_url(), 5_000, 30_000)
        .await
        .expect("instance b connects");
    for round in 0..20 {
        // A database that holds no CA yet, as on a first boot.
        sqlx::query("DELETE FROM cp_secrets")
            .execute(cp.db().pool())
            .await
            .expect("clear the secrets");
        sqlx::query("DELETE FROM cp_settings WHERE key = 'ca_cert_pem'")
            .execute(cp.db().pool())
            .await
            .expect("clear the CA certificate");
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let boot = |db: Db| {
            let barrier = barrier.clone();
            let env = cp.env().clone();
            tokio::spawn(async move {
                barrier.wait().await;
                Pki::load_or_init(&db, &MasterKey(TEST_MASTER_KEY), &env).await
            })
        };
        let (first, second) = (boot(a.clone()), boot(b.clone()));
        let first = first.await.expect("join a").expect("instance a boots");
        let second = second.await.expect("join b").expect("instance b boots");
        assert_eq!(first.ca_pem(), second.ca_pem(), "round {round}: two CAs");
        assert_eq!(
            first.platform_id_key_hex(),
            second.platform_id_key_hex(),
            "round {round}: two platform id keys"
        );

        let stored: Vec<String> =
            sqlx::query_scalar("SELECT value #>> '{}' FROM cp_settings WHERE key = 'ca_cert_pem'")
                .fetch_all(cp.db().pool())
                .await
                .expect("read the CA certificate");
        assert_eq!(stored, vec![first.ca_pem().to_string()], "round {round}");
        let sealed: Vec<u8> =
            sqlx::query_scalar("SELECT sealed FROM cp_secrets WHERE name = 'ca_key'")
                .fetch_one(cp.db().pool())
                .await
                .expect("read the sealed CA key");
        let key_der = MasterKey(TEST_MASTER_KEY)
            .open(b"aisix-cp:ca_key", &sealed)
            .expect("the CA key opens");
        let key = KeyPair::try_from(key_der.as_slice()).expect("a PKCS#8 key");
        assert_eq!(
            key.public_key_der(),
            spki_of(&stored[0]),
            "round {round}: the stored key is not the certificate's"
        );
    }
}

#[tokio::test]
async fn the_ca_key_is_stored_sealed() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let ca_pem = cp.state().pki.ca_pem().to_string();
    let sealed_of = |name: &'static str| {
        let pool = cp.db().pool().clone();
        async move {
            sqlx::query_scalar::<_, Vec<u8>>("SELECT sealed FROM cp_secrets WHERE name = $1")
                .bind(name)
                .fetch_one(&pool)
                .await
                .unwrap_or_else(|e| panic!("read {name}: {e}"))
        }
    };
    let mk = MasterKey(TEST_MASTER_KEY);

    let sealed = sealed_of("ca_key").await;
    let der = mk
        .open(b"aisix-cp:ca_key", &sealed)
        .expect("the CA key opens under its own name");
    assert!(
        !sealed.windows(der.len()).any(|w| w == der.as_slice()),
        "the CA key is stored in the clear"
    );
    assert!(!String::from_utf8_lossy(&sealed).contains("PRIVATE KEY"));
    let key = KeyPair::try_from(der.as_slice()).expect("a PKCS#8 key");
    assert_eq!(key.public_key_der(), spki_of(&ca_pem));
    // Bound to its slot and to the master key.
    assert!(mk.open(b"aisix-cp:platform_id_key", &sealed).is_err());
    assert!(MasterKey([0x11; 32])
        .open(b"aisix-cp:ca_key", &sealed)
        .is_err());

    let sealed_id = sealed_of("platform_id_key").await;
    let id_key = mk
        .open(b"aisix-cp:platform_id_key", &sealed_id)
        .expect("the platform id key opens");
    assert_eq!(id_key.len(), 32);
    assert_eq!(hex::encode(&id_key), cp.state().pki.platform_id_key_hex());
    assert!(!sealed_id.windows(32).any(|w| w == id_key.as_slice()));

    // The settings hold the certificate, and no key.
    let settings: String =
        sqlx::query_scalar("SELECT string_agg(value::text, ' ') FROM cp_settings")
            .fetch_one(cp.db().pool())
            .await
            .expect("read the settings");
    assert!(settings.contains("BEGIN CERTIFICATE"), "{settings}");
    assert!(!settings.contains("PRIVATE KEY"), "{settings}");

    // Another instance reuses the CA rather than minting one.
    let again = cp.restart().await;
    assert_eq!(again.state().pki.ca_pem(), ca_pem);
    assert_eq!(
        again.state().pki.platform_id_key_hex(),
        cp.state().pki.platform_id_key_hex()
    );

    // Under another master key the stored CA does not open, and the error
    // carries nothing of it.
    let refused = Pki::load_or_init(cp.db(), &MasterKey([0x11; 32]), cp.env()).await;
    let Err(e) = refused else {
        panic!("a CA sealed under another master key loaded");
    };
    let message = e.to_string();
    assert!(message.contains("CP_MASTER_KEY_FILE"), "{message}");
    assert!(!message.contains("PRIVATE KEY"), "{message}");
}

#[tokio::test]
async fn the_private_key_and_id_key_are_returned_once_and_never_stored_or_audited() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let r = issue(&cp, &admin, json!({"name": "gw-secret-7c1"})).await;
    assert_eq!(r.status, 201, "{r:?}");
    let cert_pem = str_at(&r.body, "/cert_pem");
    let key_pem = str_at(&r.body, "/key_pem");
    let id_key = str_at(&r.body, "/env/PLATFORM_ID_KEY");
    // A private key, the certificate's own.
    let key = KeyPair::from_pem(key_pem).expect("key_pem is a PKCS#8 key");
    assert_eq!(key.public_key_der(), spki_of(cert_pem));
    let no_store = r
        .headers
        .get(reqwest::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok());
    assert_eq!(no_store, Some("no-store"), "{:?}", r.headers);

    let key_body: String = key_pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect();
    let key_lines: Vec<&str> = key_pem
        .lines()
        .filter(|l| !l.starts_with("-----") && l.len() >= 16)
        .collect();
    let key_der_hex = hex::encode(key.serialize_der());
    let mut needles = vec![key_body.as_str(), key_der_hex.as_str(), id_key];
    needles.extend(key_lines);

    let dump = dump_all_tables(&cp).await;
    assert!(
        dump.contains("gw-secret-7c1"),
        "the dump misses the credential"
    );
    for needle in &needles {
        assert!(!dump.contains(needle), "a table holds {needle}");
    }
    assert!(!dump.contains("PRIVATE KEY"));

    let audit = audit_rows(&cp).await;
    assert!(
        audit.iter().any(|(a, _)| a == "gateway_credential.issue"),
        "{audit:?}"
    );
    for (action, detail) in &audit {
        for needle in &needles {
            assert!(!detail.contains(needle), "{action} records {needle}");
        }
    }

    // Returned once: no later read carries them.
    for path in ["/admin/gateways/credentials", "/admin/environment"] {
        let read = cp.get_with_cookie(path, admin.cookie()).await;
        assert_eq!(read.status, 200, "{path}: {read:?}");
        let text = read.body.to_string();
        assert!(!text.contains("PRIVATE KEY"), "{path}: {text}");
        for needle in &needles {
            assert!(!text.contains(needle), "{path} returns {needle}");
        }
    }
    // A second credential gets a key of its own.
    let second = issue(&cp, &admin, json!({"name": "gw-secret-7c2"})).await;
    assert_eq!(second.status, 201, "{second:?}");
    assert_ne!(str_at(&second.body, "/key_pem"), key_pem);
}

#[tokio::test]
async fn only_admins_issue_or_revoke_credentials() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let operator = cp.admin_login_seeded(OperatorRole::Operator).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let issued = issue(&cp, &admin, json!({"name": "gw-roles-7c1"})).await;
    assert_eq!(issued.status, 201, "{issued:?}");
    let dp_id = str_at(&issued.body, "/dp_id").to_string();

    for who in [&operator, &viewer] {
        let r = issue(&cp, who, json!({"name": "gw-roles-7c2"})).await;
        assert_eq!(r.status, 403, "issue as {:?}: {r:?}", who.role);
        assert_eq!(r.body["code"], "forbidden");
        assert!(!r.body.to_string().contains("PRIVATE KEY"));
        let r = revoke(&cp, who, &dp_id).await;
        assert_eq!(r.status, 403, "revoke as {:?}: {r:?}", who.role);
        assert_eq!(r.body["code"], "forbidden");
    }
    let r = cp
        .send(
            "POST",
            "/admin/gateways/credentials",
            None,
            Some(json!({"name": "gw-roles-7c3"})),
        )
        .await;
    assert_eq!(r.status, 401, "{r:?}");
    assert_eq!(credential_count(&cp).await, 1);
    let revoked: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT revoked_at FROM dp_credentials")
            .fetch_one(cp.db().pool())
            .await
            .expect("read the credential");
    assert_eq!(revoked, None, "a refused revoke revoked");

    // A viewer sees the credential, without its key.
    let list = cp
        .get_with_cookie("/admin/gateways/credentials", viewer.cookie())
        .await;
    assert_eq!(list.status, 200, "{list:?}");
    let items = list.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["dp_id"], dp_id.as_str());
    assert_eq!(items[0]["name"], "gw-roles-7c1");
    assert_eq!(items[0]["created_by"], admin.id.to_string());
    assert!(items[0]["revoked_at"].is_null());
    assert!(items[0].get("key_pem").is_none());

    let r = revoke(&cp, &admin, &dp_id).await;
    assert_eq!(r.status, 200, "{r:?}");
}

#[tokio::test]
async fn the_bundle_names_every_variable_the_image_needs_and_no_etcd_user() {
    let Some(cp) = TestCp::start_with(|c| c.etcd_prefix = "/aisix-alt-e2e".to_string()).await
    else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let r = issue(
        &cp,
        &admin,
        json!({"name": "gw-bundle-7c1", "validity_days": 30}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let env = r.body["env"].as_object().expect("env is an object");
    let names: BTreeSet<&str> = env.keys().map(String::as_str).collect();
    assert_eq!(names, BTreeSet::from(BUNDLE_VARS), "{names:?}");
    assert!(env.values().all(Value::is_string), "{env:?}");
    let var = |name: &str| env[name].as_str().expect("a string");

    assert_eq!(var("AISIX_CONFIG_PATH"), "/etc/aisix/config.managed.yaml");
    assert_eq!(var("AISIX_MANAGED__ENABLED"), "true");
    assert_eq!(var("AISIX_MANAGED__CP_BASE_URL"), "https://localhost:7943");
    assert_eq!(var("AISIX_MANAGED__CP_ETCD_ENDPOINT"), "localhost:7943");
    assert_eq!(
        var("AISIX_MANAGED__CP_CERT_PEM"),
        str_at(&r.body, "/cert_pem")
    );
    assert_eq!(
        var("AISIX_MANAGED__CP_KEY_PEM"),
        str_at(&r.body, "/key_pem")
    );
    assert_eq!(var("AISIX_MANAGED__CP_CA_PEM"), str_at(&r.body, "/ca_pem"));
    assert_eq!(var("AISIX_ETCD__PREFIX"), "/aisix-alt-e2e");
    assert_eq!(var("AISIX_MANAGED__SNAPSHOT_CACHE_ENABLED"), "true");
    assert_eq!(var("AISIX_PROXY__PLATFORM__ENABLED"), "true");
    assert_eq!(var("AISIX_PROXY__PLATFORM__ID_KEY_ENV"), "PLATFORM_ID_KEY");
    let id_key = var("PLATFORM_ID_KEY");
    assert_eq!(id_key.len(), 64, "{id_key}");
    assert!(id_key.bytes().all(|b| b.is_ascii_hexdigit()), "{id_key}");
    for absent in ["AISIX_ETCD__USER", "AISIX_ETCD__PASSWORD_ENV"] {
        assert!(!env.contains_key(absent), "{absent}");
    }
    // The bundle's note says what to protect.
    let note = str_at(&r.body, "/note");
    assert!(note.contains("/var/lib/aisix"), "{note}");

    // The environment view reports the prefix the bundle carries.
    let view = cp
        .get_with_cookie("/admin/environment", admin.cookie())
        .await;
    assert_eq!(view.status, 200, "{view:?}");
    assert_eq!(view.body["etcd_prefix"], "/aisix-alt-e2e");

    // One platform id key for every gateway of the environment.
    let again = issue(&cp, &admin, json!({"name": "gw-bundle-7c2"})).await;
    assert_eq!(again.status, 201, "{again:?}");
    assert_eq!(str_at(&again.body, "/env/PLATFORM_ID_KEY"), id_key);
    assert_ne!(
        str_at(&again.body, "/dp_id"),
        str_at(&r.body, "/dp_id"),
        "two credentials share a dp_id"
    );
}

#[tokio::test]
async fn an_expired_ca_issues_nothing_and_says_so() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    // The stored CA replaced by one that expired 30 days ago, and an
    // instance booted on it.
    let now = time::OffsetDateTime::now_utc();
    let mut params = rcgen::CertificateParams::default();
    let mut dn = rcgen::DistinguishedName::new();
    dn.push(
        rcgen::DnType::CommonName,
        "AISIX control plane CA expired-7c1",
    );
    params.distinguished_name = dn;
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    params.not_before = now - time::Duration::days(3680);
    params.not_after = now - time::Duration::days(30);
    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("a key");
    let ca = params.self_signed(&key).expect("an expired CA");
    sqlx::query("UPDATE cp_settings SET value = to_jsonb($1::text) WHERE key = 'ca_cert_pem'")
        .bind(ca.pem())
        .execute(cp.db().pool())
        .await
        .expect("store the expired CA");
    sqlx::query("UPDATE cp_secrets SET sealed = $1 WHERE name = 'ca_key'")
        .bind(MasterKey(TEST_MASTER_KEY).seal(b"aisix-cp:ca_key", &key.serialize_der()))
        .execute(cp.db().pool())
        .await
        .expect("store its key");
    let expired = cp.restart().await;
    assert_eq!(expired.state().pki.ca_pem(), ca.pem());

    let admin = expired.admin_login_seeded(OperatorRole::Admin).await;
    let r = issue(&expired, &admin, json!({"name": "gw-expired-7c1"})).await;
    assert_eq!(r.status, 503, "{r:?}");
    assert_eq!(r.body["code"], "ca_expired");
    let message = str_at(&r.body, "/error");
    assert!(
        message.contains("CA") && message.contains("过期"),
        "{message}"
    );
    let text = r.body.to_string();
    assert!(
        !text.contains("PRIVATE KEY") && !text.contains("BEGIN CERTIFICATE"),
        "{text}"
    );
    // Nothing issued, nothing recorded.
    assert_eq!(credential_count(&expired).await, 0);
    assert!(
        !audit_rows(&expired)
            .await
            .iter()
            .any(|(action, _)| action == "gateway_credential.issue"),
        "an issue was audited"
    );
}

#[tokio::test]
async fn validity_days_defaults_to_365_and_is_bounded() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    // Days from now to not_after, which must be the certificate's own.
    let days_of = |body: &Value| {
        let der = der_of(str_at(body, "/cert_pem"));
        let (_, cert) = X509Certificate::from_der(&der).expect("a certificate");
        let not_after = chrono::DateTime::parse_from_rfc3339(str_at(body, "/not_after"))
            .expect("not_after is RFC 3339")
            .timestamp();
        assert_eq!(not_after, cert.validity().not_after.timestamp());
        (not_after - chrono::Utc::now().timestamp()) as f64 / 86_400.0
    };
    let r = issue(&cp, &admin, json!({"name": "gw-days-7c1"})).await;
    assert_eq!(r.status, 201, "{r:?}");
    let days = days_of(&r.body);
    assert!((364.9..=365.1).contains(&days), "{days}");
    let r = issue(
        &cp,
        &admin,
        json!({"name": "gw-days-7c2", "validity_days": 825}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let days = days_of(&r.body);
    assert!((824.9..=825.1).contains(&days), "{days}");

    for (body, path) in [
        (
            json!({"name": "gw-days-7c3", "validity_days": 0}),
            "/validity_days",
        ),
        (
            json!({"name": "gw-days-7c4", "validity_days": 826}),
            "/validity_days",
        ),
        (json!({"name": "   "}), "/name"),
        (json!({"name": "n".repeat(129)}), "/name"),
    ] {
        let r = issue(&cp, &admin, body.clone()).await;
        assert_eq!(r.status, 422, "{body}: {r:?}");
        assert_eq!(r.body["code"], "validation_failed");
        assert_eq!(r.body["details"][0]["path"], path, "{body}: {r:?}");
    }
    assert_eq!(credential_count(&cp).await, 2);
}

/// The payload of the next notification, within 10 seconds.
async fn next_notice(listener: &mut sqlx::postgres::PgListener) -> String {
    tokio::time::timeout(Duration::from_secs(10), listener.recv())
        .await
        .expect("a notification within 10 s")
        .expect("receive a notification")
        .payload()
        .to_string()
}

#[tokio::test]
async fn revoking_a_credential_is_recorded_and_audited() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let mut listener = sqlx::postgres::PgListener::connect(cp.database_url())
        .await
        .expect("connect a listener");
    listener
        .listen("dp_credentials_changed")
        .await
        .expect("listen for credential changes");

    let issued = issue(&cp, &admin, json!({"name": "gw-revoke-7c1"})).await;
    assert_eq!(issued.status, 201, "{issued:?}");
    let dp_id = str_at(&issued.body, "/dp_id").to_string();
    // An issue is a change too: a gateway may connect at once.
    assert_eq!(next_notice(&mut listener).await, dp_id);

    let r = revoke(&cp, &admin, &dp_id).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["dp_id"], dp_id.as_str());
    assert_eq!(r.body["name"], "gw-revoke-7c1");
    let revoked_at = str_at(&r.body, "/revoked_at").to_string();
    assert_eq!(next_notice(&mut listener).await, dp_id);
    let stored: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT revoked_at FROM dp_credentials WHERE dp_id = $1::uuid")
            .bind(&dp_id)
            .fetch_one(cp.db().pool())
            .await
            .expect("read the credential");
    assert!(stored.is_some(), "the revoke was not stored");

    let revokes = || async {
        sqlx::query_as::<_, (String, Option<Uuid>, String)>(
            "SELECT target_id, actor_id, detail::text FROM audit_log \
             WHERE action = 'gateway_credential.revoke'",
        )
        .fetch_all(cp.db().pool())
        .await
        .expect("read the audit log")
    };
    let rows = revokes().await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, dp_id);
    assert_eq!(rows[0].1, Some(admin.id));
    assert!(rows[0].2.contains("gw-revoke-7c1"), "{rows:?}");

    // A second revoke changes nothing and records nothing.
    let again = revoke(&cp, &admin, &dp_id).await;
    assert_eq!(again.status, 200, "{again:?}");
    assert_eq!(str_at(&again.body, "/revoked_at"), revoked_at);
    assert_eq!(revokes().await.len(), 1);

    // The list shows it revoked.
    let list = cp
        .get_with_cookie("/admin/gateways/credentials", admin.cookie())
        .await;
    assert_eq!(list.body["items"][0]["revoked_at"], revoked_at.as_str());

    for missing in [Uuid::new_v4().to_string(), "not-a-uuid".to_string()] {
        let r = revoke(&cp, &admin, &missing).await;
        assert_eq!(r.status, 404, "{missing}: {r:?}");
        assert_eq!(r.body["code"], "not_found");
    }
}

#[tokio::test]
async fn the_environment_view_shows_what_the_bundle_is_built_from() {
    // A URL is used as configured and never displayed that way.
    let Some(cp) = TestCp::start_with(|c| {
        c.dp_base_url = "https://gw-user:pw-env-7c1@localhost:7943".to_string()
    })
    .await
    else {
        return;
    };
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let r = cp
        .get_with_cookie("/admin/environment", viewer.cookie())
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["env_id"], cp.env().env_id.to_string());
    assert_eq!(r.body["etcd_prefix"], "/aisix");
    assert_eq!(r.body["etcd_endpoint"], "localhost:7943");
    assert_eq!(r.body["api_contract"], 1);
    assert!(
        r.body["cp_version"].as_str().is_some_and(|v| !v.is_empty()),
        "{r:?}"
    );
    let shown = str_at(&r.body, "/dp_base_url");
    assert!(shown.starts_with("https://localhost:7943"), "{shown}");
    assert!(!r.body.to_string().contains("pw-env-7c1"), "{r:?}");

    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let bundle = issue(&cp, &admin, json!({"name": "gw-env-7c1"})).await;
    assert_eq!(bundle.status, 201, "{bundle:?}");
    assert_eq!(
        str_at(&bundle.body, "/env/AISIX_MANAGED__CP_BASE_URL"),
        "https://gw-user:pw-env-7c1@localhost:7943"
    );
    assert_eq!(
        str_at(&bundle.body, "/env/AISIX_MANAGED__CP_ETCD_ENDPOINT"),
        "localhost:7943"
    );
}

/// Marks the child process [`a_gateway_provisions_and_connects_from_the_bundle`]
/// runs the gateway's configuration loader in. Not an `AISIX_*` name: the
/// loader would report it.
const LOADER_CHILD: &str = "CP_TEST_GATEWAY_LOADER_CHILD";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root")
}

/// In the child: the gateway's own loader reads the image's managed
/// configuration file, with the bundle's variables as the only `AISIX_*`
/// ones in the environment.
fn load_as_the_gateway() {
    let path = repo_root().join("config.managed.yaml");
    let cfg = aisix_core::Config::load_from_path(Some(&path))
        .unwrap_or_else(|e| panic!("the gateway refuses the bundle: {e}"));
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is unset"));
    assert!(cfg.managed.enabled);
    assert_eq!(
        cfg.managed.cp_base_url.as_deref(),
        Some(var("AISIX_MANAGED__CP_BASE_URL").as_str())
    );
    assert_eq!(
        cfg.managed.cp_etcd_endpoint.as_deref(),
        Some(var("AISIX_MANAGED__CP_ETCD_ENDPOINT").as_str())
    );
    assert_eq!(
        cfg.managed.cp_cert_pem.as_deref(),
        Some(var("AISIX_MANAGED__CP_CERT_PEM").as_str())
    );
    assert_eq!(
        cfg.managed.cp_key_pem.as_deref(),
        Some(var("AISIX_MANAGED__CP_KEY_PEM").as_str())
    );
    assert_eq!(
        cfg.managed.cp_ca_pem.as_deref(),
        Some(var("AISIX_MANAGED__CP_CA_PEM").as_str())
    );
    assert_eq!(cfg.etcd.prefix, var("AISIX_ETCD__PREFIX"));
    assert!(cfg.etcd.user.is_none() && cfg.etcd.password_env.is_none());
    assert!(cfg.managed.snapshot_cache_enabled);
    assert!(cfg.proxy.platform.enabled);
    assert_eq!(
        cfg.proxy.platform.id_key_env.as_deref(),
        Some("PLATFORM_ID_KEY")
    );
    assert_eq!(cfg.ignored_keys(), Vec::<String>::new());
    assert_eq!(
        aisix_core::Config::ignored_env_overrides(),
        Vec::<String>::new()
    );
}

/// Runs [`load_as_the_gateway`] in a child process whose only `AISIX_*`
/// variables are the bundle's: the loader reads the process environment, and
/// this process's must stay as it is.
fn load_in_a_child(env: &serde_json::Map<String, Value>) {
    let mut child = std::process::Command::new(std::env::current_exe().expect("the test binary"));
    child
        .args([
            "--exact",
            "a_gateway_provisions_and_connects_from_the_bundle",
            "--test-threads=1",
        ])
        .env(LOADER_CHILD, "1");
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("AISIX_")
        {
            child.env_remove(&name);
        }
    }
    for (name, value) in env {
        child.env(name, value.as_str().expect("a string value"));
    }
    let out = child.output().expect("run the loader child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "the gateway's loader refused the bundle:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("1 passed"),
        "the child ran nothing: {stdout}"
    );
}

/// A temporary directory, removed on drop.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Accepts one mTLS connection, answers one HTTP/1.1 request with `200 {}`,
/// and returns the client's leaf certificate.
async fn answer_one_request(
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
) -> Vec<u8> {
    let (tcp, _) = listener.accept().await.expect("accept");
    let mut tls = acceptor.accept(tcp).await.expect("the mTLS handshake");
    let leaf = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .expect("a client certificate")
        .to_vec();
    let mut head = Vec::new();
    let mut chunk = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tls.read(&mut chunk).await.expect("read the request");
        assert!(n > 0, "the request ended early");
        head.extend_from_slice(&chunk[..n]);
    }
    tls.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\
          connection: close\r\n\r\n{}",
    )
    .await
    .expect("answer");
    let _ = tls.shutdown().await;
    leaf
}

#[tokio::test]
async fn a_gateway_provisions_and_connects_from_the_bundle() {
    if std::env::var_os(LOADER_CHILD).is_some() {
        load_as_the_gateway();
        return;
    }
    // The gateway listener's stand-in, on the port the bundle names.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("an address").port();
    let Some(cp) =
        TestCp::start_with(|c| c.dp_base_url = format!("https://localhost:{port}")).await
    else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let r = issue(&cp, &admin, json!({"name": "gw-boot-7c1"})).await;
    assert_eq!(r.status, 201, "{r:?}");
    let env = r.body["env"].as_object().expect("env").clone();
    let var = |name: &str| env[name].as_str().expect("a string").to_string();
    let dp_id = str_at(&r.body, "/dp_id").to_string();

    // cert_bundle.rs `provision`: the three PEMs land in the mTLS directory
    // under these names, and the leaf's SANs give env_id and dp_id.
    let dir = TempDir(std::env::temp_dir().join(format!("aisix-cp-mtls-{}", Uuid::new_v4())));
    std::fs::create_dir_all(&dir.0).expect("create the mTLS directory");
    for (file, name) in [
        ("ca.crt", "AISIX_MANAGED__CP_CA_PEM"),
        ("client.crt", "AISIX_MANAGED__CP_CERT_PEM"),
        ("client.key", "AISIX_MANAGED__CP_KEY_PEM"),
    ] {
        std::fs::write(dir.0.join(file), var(name)).expect("write a bundle file");
    }
    let read = |file: &str| std::fs::read(dir.0.join(file)).expect("read a bundle file");
    let client_crt = String::from_utf8(read("client.crt")).expect("UTF-8");
    let (env_id, cert_dp_id) = gateway_san_uris(&client_crt);
    assert_eq!(env_id, cp.env().env_id.to_string());
    // A restart re-reads env_id from disk and requires a UUID
    // (managed_bundle.rs).
    Uuid::parse_str(&env_id).expect("env_id is a UUID");
    assert_eq!(cert_dp_id, dp_id);

    // heartbeat.rs `build_client`: key then certificate as one identity, the
    // CA as a root, HTTP/1.1 only.
    let mut identity_pem = read("client.key");
    if !identity_pem.ends_with(b"\n") {
        identity_pem.push(b'\n');
    }
    identity_pem.extend_from_slice(&read("client.crt"));
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .identity(reqwest::Identity::from_pem(&identity_pem).expect("an mTLS identity"))
        .add_root_certificate(reqwest::Certificate::from_pem(&read("ca.crt")).expect("the CA"))
        .http1_only()
        .use_rustls_tls()
        .build()
        .expect("the gateway's REST client");

    // The control plane's side: its own server certificate for CP_DP_HOSTS,
    // and gateway certificates required, rooted at its CA.
    let pki = &cp.state().pki;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(der_of(pki.ca_pem()).into())
        .expect("the CA as a root");
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        provider.clone(),
    )
    .build()
    .expect("a client verifier");
    let (chain, key) = pki
        .server_cert(&cp.state().cfg.dp_hosts)
        .expect("a server certificate");
    let mut server = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, key)
        .expect("the server certificate and key");
    server.alpn_protocols = vec![b"http/1.1".to_vec()];
    let served = tokio::spawn(answer_one_request(
        listener,
        tokio_rustls::TlsAcceptor::from(Arc::new(server)),
    ));

    let base = var("AISIX_MANAGED__CP_BASE_URL");
    let answer = client
        .post(format!("{}/dp/heartbeat", base.trim_end_matches('/')))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("the heartbeat reaches the control plane over mTLS");
    assert_eq!(answer.status(), 200);
    let leaf = served.await.expect("the server task");
    let identity = pki::identity_of(&leaf).expect("the gateway's identity");
    assert_eq!(identity.env_id, cp.env().env_id.to_string());
    assert_eq!(identity.dp_id, dp_id);
    let list = cp
        .get_with_cookie("/admin/gateways/credentials", admin.cookie())
        .await;
    assert_eq!(
        list.body["items"][0]["fingerprint_sha256"],
        identity.fingerprint_sha256.as_str()
    );

    // The official image boots with AISIX_CONFIG_PATH: the entrypoint exits
    // without a file there, and the managed file is baked at that path.
    let dockerfile =
        std::fs::read_to_string(repo_root().join("Dockerfile")).expect("read the Dockerfile");
    let baked = format!("COPY config.managed.yaml {}", var("AISIX_CONFIG_PATH"));
    assert!(dockerfile.contains(&baked), "the image bakes no {baked}");
    // And the gateway's loader accepts every variable, ignoring none.
    load_in_a_child(&env);
}
