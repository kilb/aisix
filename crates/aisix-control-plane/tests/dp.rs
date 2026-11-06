//! The gateway listener: mTLS on `CP_DP_ADDR`, heartbeats and the instance
//! list, the `/dp/store/v1` stub, revocation of a credential in use, and
//! the housekeeping that prunes what is left behind.
//!
//! Heartbeat bodies have the shape the gateway sends
//! (`HeartbeatBody` in `crates/aisix-server/src/heartbeat.rs`), and the
//! gateway's REST client is built the way it builds it (`support::DpClient`).

mod support;

use std::sync::Arc;
use std::time::Duration;

use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::dp::{listener, ConnIdentity, CredentialCache};
use aisix_control_plane::gateways::version_matches;
use aisix_control_plane::housekeeping;
use aisix_control_plane::pki::Pki;
use aisix_control_plane::settings::EnvSettings;
use axum::routing::get;
use axum::{Extension, Router};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
    SanType, PKCS_ECDSA_P256_SHA256,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use serde_json::{json, Value};
use support::{Bundle, DpClient, OperatorSession, TestCp};
use uuid::Uuid;
use x509_parser::pem::parse_x509_pem;

/// A heartbeat as the gateway sends it, every field present.
fn full_beat(dp_id: &str, instance_id: &str) -> Value {
    json!({
        "dp_id": dp_id,
        "instance_id": instance_id,
        "hostname": "gw-host-7c1",
        "uptime_seconds": 42,
        "version": "1.9.0+sha-abc1234",
        "supported_guardrail_kinds": ["keyword", "regex"],
        "applied_revision": 1234,
        "config_hash": "ab".repeat(32),
        "exporter_health": [{
            "name": "otlp-7c1",
            "delivered_batches": 5,
            "failed_batches": 1,
            "last_error": "timeout-7c1",
            "last_failure_unix": 1_700_000_000,
            "last_success_unix": 1_700_000_100,
        }],
        "rejected_resources": [{
            "key": "/aisix/env/models/m-7c1",
            "kind": "schema_failed",
            "error": "missing field model-7c1",
            "timestamp_unix_secs": 1_700_000_000u64,
        }],
        "partially_compatible_resources": [
            {"kind": "models", "field": "future_knob_7c1", "count": 2}
        ],
    })
}

/// A heartbeat with only the fields every gateway sends, and empty lists.
fn bare_beat(dp_id: &str, instance_id: &str) -> Value {
    json!({
        "dp_id": dp_id,
        "instance_id": instance_id,
        "hostname": "gw-host-7c2",
        "uptime_seconds": 1,
        "version": "1.9.0+sha-abc1234",
        "supported_guardrail_kinds": [],
        "applied_revision": 7,
        "exporter_health": [],
    })
}

async fn setup(cp: &TestCp) -> (OperatorSession, Bundle) {
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let bundle = cp.issue_bundle(&admin, "gw-dp-7c1").await;
    (admin, bundle)
}

async fn instances(cp: &TestCp, who: &OperatorSession) -> Vec<Value> {
    let r = cp
        .get_with_cookie("/admin/gateways/instances", who.cookie())
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    r.body["items"].as_array().expect("items").clone()
}

async fn instance_rows(cp: &TestCp) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM dp_instances")
        .fetch_one(cp.db().pool())
        .await
        .expect("count instances")
}

/// `(rejected_resources, partially_compatible_resources)` as stored.
async fn stored_lists(cp: &TestCp, instance_id: &str) -> (Value, Value) {
    sqlx::query_as(
        "SELECT rejected_resources, partially_compatible_resources FROM dp_instances \
         WHERE instance_id = $1",
    )
    .bind(instance_id)
    .fetch_one(cp.db().pool())
    .await
    .expect("read the instance")
}

fn der_of(pem: &str) -> Vec<u8> {
    parse_x509_pem(pem.as_bytes())
        .expect("a PEM certificate")
        .1
        .contents
}

/// One TLS connection to `addr` as `cert_pem`/`key_pem`, trusting
/// `ca_pem`, offering `alpn`.
async fn tls_connect(
    addr: std::net::SocketAddr,
    (cert_pem, key_pem, ca_pem): (&str, &str, &str),
    alpn: &[&[u8]],
) -> std::io::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(der_of(ca_pem)))
        .expect("the CA as a root");
    let key = KeyPair::from_pem(key_pem).expect("a PKCS#8 key");
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_root_certificates(roots)
        .with_client_auth_cert(
            vec![CertificateDer::from(der_of(cert_pem))],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .expect("a client identity");
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let tcp = tokio::net::TcpStream::connect(addr).await?;
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").expect("a name"), tcp)
        .await
}

fn pems(bundle: &Bundle) -> (&str, &str, &str) {
    (&bundle.cert_pem, &bundle.key_pem, &bundle.ca_pem)
}

/// One HTTP/1.1 connection, kept open across requests: every request goes
/// over it, or fails.
struct DpConn {
    sender: hyper::client::conn::http1::SendRequest<Full<Bytes>>,
}

impl DpConn {
    async fn open(cp: &TestCp, bundle: &Bundle) -> DpConn {
        let tls = tls_connect(cp.dp_addr(), pems(bundle), &[b"http/1.1"])
            .await
            .expect("the mTLS handshake");
        let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .expect("an HTTP/1.1 connection");
        tokio::spawn(async move {
            let _ = conn.await;
        });
        DpConn { sender }
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        assert!(!self.sender.is_closed(), "the connection was closed");
        self.sender
            .ready()
            .await
            .expect("the connection is still open");
        let bytes = body.map_or_else(Vec::new, |b| serde_json::to_vec(b).expect("JSON"));
        let request = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(bytes)))
            .expect("a request");
        let response = self
            .sender
            .send_request(request)
            .await
            .expect("an answer on the same connection");
        let status = response.status().as_u16();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("read the body")
            .to_bytes();
        let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
        (status, body)
    }
}

#[tokio::test]
async fn a_heartbeat_records_the_instance_and_its_applied_revision() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (_, bundle) = setup(&cp).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let gw = cp.dp_client(&bundle);
    let instance = Uuid::new_v4().to_string();

    let r = gw.heartbeat(full_beat(&bundle.dp_id, &instance)).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body, json!({}));

    let items = instances(&cp, &viewer).await;
    assert_eq!(items.len(), 1, "{items:?}");
    let item = &items[0];
    assert_eq!(item["dp_id"], bundle.dp_id.as_str());
    assert_eq!(item["credential_name"], "gw-dp-7c1");
    assert_eq!(item["instance_id"], instance.as_str());
    assert_eq!(item["hostname"], "gw-host-7c1");
    assert_eq!(item["version"], "1.9.0+sha-abc1234");
    assert_eq!(item["uptime_seconds"], 42);
    assert_eq!(item["applied_revision"], 1234);
    assert_eq!(item["config_hash"], "ab".repeat(32));
    assert_eq!(item["online"], true);
    assert!(item["last_seen_at"].is_string(), "{item}");
    assert_eq!(
        item["exporter_health"],
        full_beat("", "")["exporter_health"]
    );
    assert_eq!(item["rejected_count"], 1);
    assert!(
        item.get("version_matches_cp").is_some(),
        "version_matches_cp is always present: {item}"
    );
    let (rejected, partial) = stored_lists(&cp, &instance).await;
    assert_eq!(rejected, full_beat("", "")["rejected_resources"]);
    assert_eq!(partial, full_beat("", "")["partially_compatible_resources"]);

    // A later beat moves the instance on; it stays one instance, first seen
    // when it was.
    let first: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as("SELECT first_seen_at, last_seen_at FROM dp_instances")
            .fetch_one(cp.db().pool())
            .await
            .expect("read the instance");
    tokio::time::sleep(Duration::from_millis(20)).await;
    let mut later = full_beat(&bundle.dp_id, &instance);
    later["applied_revision"] = json!(1300);
    later["uptime_seconds"] = json!(57);
    let r = gw.heartbeat(later).await;
    assert_eq!(r.status, 200, "{r:?}");
    let items = instances(&cp, &viewer).await;
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["applied_revision"], 1300);
    assert_eq!(items[0]["uptime_seconds"], 57);
    let second: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as("SELECT first_seen_at, last_seen_at FROM dp_instances")
            .fetch_one(cp.db().pool())
            .await
            .expect("read the instance");
    assert_eq!(second.0, first.0, "first_seen_at moved");
    assert!(second.1 > first.1, "last_seen_at did not move: {second:?}");
}

#[tokio::test]
async fn an_instance_on_the_control_planes_own_version_matches_when_that_can_be_told() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (admin, bundle) = setup(&cp).await;
    let gw = cp.dp_client(&bundle);
    let cp_version = cp
        .get_with_cookie("/admin/environment", admin.cookie())
        .await
        .body["cp_version"]
        .as_str()
        .expect("cp_version")
        .to_string();
    let same = Uuid::new_v4().to_string();
    let mut beat = bare_beat(&bundle.dp_id, &same);
    beat["version"] = json!(cp_version);
    assert_eq!(gw.heartbeat(beat).await.status, 200);
    let garbled = Uuid::new_v4().to_string();
    let mut beat = bare_beat(&bundle.dp_id, &garbled);
    beat["version"] = json!("not-a-version-7c1");
    assert_eq!(gw.heartbeat(beat).await.status, 200);

    let items = instances(&cp, &admin).await;
    let matches_of = |id: &str| {
        items
            .iter()
            .find(|i| i["instance_id"] == id)
            .unwrap_or_else(|| panic!("{id} in {items:?}"))["version_matches_cp"]
            .clone()
    };
    // An unstamped build ("dev") cannot be compared with anything.
    let expected = if cp_version == "dev" {
        Value::Null
    } else {
        json!(true)
    };
    assert_eq!(matches_of(&same), expected, "cp {cp_version}");
    assert_eq!(matches_of(&garbled), Value::Null);
}

#[tokio::test]
async fn two_replicas_sharing_a_credential_are_two_instances() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (admin, bundle) = setup(&cp).await;
    let (a, b) = (Uuid::new_v4().to_string(), Uuid::new_v4().to_string());
    let mut beat_a = bare_beat(&bundle.dp_id, &a);
    beat_a["hostname"] = json!("replica-a-7c1");
    let mut beat_b = bare_beat(&bundle.dp_id, &b);
    beat_b["hostname"] = json!("replica-b-7c1");
    // Each replica has a client of its own, as two processes would.
    assert_eq!(
        cp.dp_client(&bundle).heartbeat(beat_a.clone()).await.status,
        200
    );
    assert_eq!(cp.dp_client(&bundle).heartbeat(beat_b).await.status, 200);
    assert_eq!(cp.dp_client(&bundle).heartbeat(beat_a).await.status, 200);

    let items = instances(&cp, &admin).await;
    assert_eq!(items.len(), 2, "{items:?}");
    assert!(items.iter().all(|i| i["dp_id"] == bundle.dp_id.as_str()));
    let mut hosts: Vec<(&str, &str)> = items
        .iter()
        .map(|i| {
            (
                i["instance_id"].as_str().expect("instance_id"),
                i["hostname"].as_str().expect("hostname"),
            )
        })
        .collect();
    hosts.sort();
    let mut expected = vec![(a.as_str(), "replica-a-7c1"), (b.as_str(), "replica-b-7c1")];
    expected.sort();
    assert_eq!(hosts, expected);
}

#[tokio::test]
async fn a_later_beat_replaces_the_rejections_it_reports() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (admin, bundle) = setup(&cp).await;
    let gw = cp.dp_client(&bundle);
    let instance = Uuid::new_v4().to_string();
    let rejection = |key: &str| json!({"key": key, "kind": "parse_failed", "error": "bad-7c1", "timestamp_unix_secs": 1});

    let mut beat = full_beat(&bundle.dp_id, &instance);
    beat["rejected_resources"] = json!([rejection("/k/one-7c1"), rejection("/k/two-7c1")]);
    assert_eq!(gw.heartbeat(beat).await.status, 200);
    assert_eq!(instances(&cp, &admin).await[0]["rejected_count"], 2);

    // The next beat reports one rejection and no partial compatibility (the
    // gateway omits an empty list): what it no longer reports is gone.
    let mut beat = bare_beat(&bundle.dp_id, &instance);
    beat["rejected_resources"] = json!([rejection("/k/three-7c1")]);
    assert_eq!(gw.heartbeat(beat).await.status, 200);
    assert_eq!(instances(&cp, &admin).await[0]["rejected_count"], 1);
    let (rejected, partial) = stored_lists(&cp, &instance).await;
    assert_eq!(rejected, json!([rejection("/k/three-7c1")]));
    assert_eq!(partial, json!([]));

    assert_eq!(
        gw.heartbeat(bare_beat(&bundle.dp_id, &instance))
            .await
            .status,
        200
    );
    assert_eq!(instances(&cp, &admin).await[0]["rejected_count"], 0);
    assert_eq!(stored_lists(&cp, &instance).await, (json!([]), json!([])));
}

#[tokio::test]
async fn a_heartbeat_with_fields_this_build_does_not_know_is_accepted() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (admin, bundle) = setup(&cp).await;
    let gw = cp.dp_client(&bundle);

    // Fields a newer gateway may send, at the top and inside a list entry.
    let newer = Uuid::new_v4().to_string();
    let mut beat = full_beat(&bundle.dp_id, &newer);
    beat["future_field_7c1"] = json!({"nested": [1, 2, 3]});
    beat["rejected_resources"][0]["future_entry_field_7c1"] = json!(true);
    let r = gw.heartbeat(beat).await;
    assert_eq!(r.status, 200, "{r:?}");

    // Only the two identifying fields: the rest has defaults.
    let minimal = Uuid::new_v4().to_string();
    let r = gw
        .heartbeat(json!({"dp_id": bundle.dp_id, "instance_id": minimal}))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let items = instances(&cp, &admin).await;
    assert_eq!(items.len(), 2, "{items:?}");
    let item = items
        .iter()
        .find(|i| i["instance_id"] == minimal.as_str())
        .expect("the minimal instance");
    assert_eq!(item["hostname"], "");
    assert_eq!(item["version"], "");
    assert_eq!(item["uptime_seconds"], 0);
    assert_eq!(item["applied_revision"], 0);
    assert_eq!(item["config_hash"], Value::Null);
    assert_eq!(item["exporter_health"], json!([]));
    assert_eq!(item["rejected_count"], 0);
    assert_eq!(item["version_matches_cp"], Value::Null);
    assert_eq!(item["online"], true);

    // Without either identifying field it is not a heartbeat.
    for body in [
        json!({"dp_id": bundle.dp_id}),
        json!({"instance_id": Uuid::new_v4().to_string()}),
        json!({"dp_id": bundle.dp_id, "instance_id": ""}),
    ] {
        let r = gw.heartbeat(body.clone()).await;
        assert_eq!(r.status, 422, "{body}: {r:?}");
        assert_eq!(r.body["code"], "validation_failed", "{body}");
    }
    assert_eq!(instance_rows(&cp).await, 2);
}

#[tokio::test]
async fn a_heartbeat_over_one_mib_is_refused() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (_, bundle) = setup(&cp).await;
    let mut beat = bare_beat(&bundle.dp_id, &Uuid::new_v4().to_string());
    beat["hostname"] = json!("h".repeat(1 << 20));
    let r = cp.dp_client(&bundle).heartbeat(beat).await;
    assert_eq!(r.status, 413, "{}", r.status);
    assert_eq!(r.body["code"], "payload_too_large");
    assert_eq!(instance_rows(&cp).await, 0);
}

#[tokio::test]
async fn a_body_naming_another_dp_id_is_403() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (admin, mine) = setup(&cp).await;
    let theirs = cp.issue_bundle(&admin, "gw-dp-7c2").await;
    let gw = cp.dp_client(&mine);
    for dp_id in [theirs.dp_id.as_str(), "not-a-uuid-7c1", ""] {
        let r = gw
            .heartbeat(bare_beat(dp_id, &Uuid::new_v4().to_string()))
            .await;
        assert_eq!(r.status, 403, "dp_id {dp_id:?}: {r:?}");
        assert_eq!(r.body["code"], "forbidden");
    }
    assert_eq!(instance_rows(&cp).await, 0);
    // Its own dp_id, in any case, is accepted.
    let r = gw
        .heartbeat(bare_beat(
            &mine.dp_id.to_ascii_uppercase(),
            &Uuid::new_v4().to_string(),
        ))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        instances(&cp, &admin).await[0]["dp_id"],
        mine.dp_id.as_str()
    );
}

#[tokio::test]
async fn a_revoked_credential_is_refused_on_an_open_connection() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (admin, bundle) = setup(&cp).await;
    let instance = Uuid::new_v4().to_string();
    let mut conn = DpConn::open(&cp, &bundle).await;
    let (status, _) = conn
        .send(
            "POST",
            "/dp/heartbeat",
            Some(&bare_beat(&bundle.dp_id, &instance)),
        )
        .await;
    assert_eq!(status, 200);

    let r = cp
        .send(
            "POST",
            &format!("/admin/gateways/credentials/{}/revoke", bundle.dp_id),
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");

    // The same connection, still open, is refused once the revoke is
    // known: well within the periodic refresh, so it is the notification
    // that told this instance.
    let mut beat = bare_beat(&bundle.dp_id, &instance);
    beat["applied_revision"] = json!(999);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let refused = loop {
        let (status, body) = conn.send("POST", "/dp/heartbeat", Some(&beat)).await;
        if status != 200 {
            break (status, body);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "still accepted 5 s after the revoke"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(refused.0, 403, "{refused:?}");
    assert_eq!(refused.1["code"], "forbidden");
    let (status, _) = conn
        .send("GET", "/dp/store/v1/mcp_oauth_token/k-7c1", None)
        .await;
    assert_eq!(status, 403);
    // A refused beat records nothing.
    let mut refused_beat = bare_beat(&bundle.dp_id, &instance);
    refused_beat["applied_revision"] = json!(4242);
    let (status, _) = conn
        .send("POST", "/dp/heartbeat", Some(&refused_beat))
        .await;
    assert_eq!(status, 403);
    let applied: i64 = sqlx::query_scalar("SELECT applied_revision FROM dp_instances")
        .fetch_one(cp.db().pool())
        .await
        .expect("read the instance");
    assert_ne!(applied, 4242);

    // A new connection with it is refused too.
    let r = cp.dp_client(&bundle).heartbeat(beat).await;
    assert_eq!(r.status, 403, "{r:?}");
}

/// A CA of its own, and a leaf of it that names `env_id` and `dp_id` the
/// way a gateway certificate does.
fn foreign_identity(env_id: &str, dp_id: &str) -> (String, String) {
    let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
    let mut ca = CertificateParams::default();
    ca.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    let ca = ca.self_signed(&ca_key).expect("a CA");
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
    let mut leaf = CertificateParams::default();
    leaf.subject_alt_names = vec![
        SanType::URI(format!("x-aisix://env/{env_id}").try_into().expect("a URI")),
        SanType::URI(format!("x-aisix://dp/{dp_id}").try_into().expect("a URI")),
    ];
    leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let leaf = leaf.signed_by(&key, &ca, &ca_key).expect("a leaf");
    (leaf.pem(), key.serialize_pem())
}

#[tokio::test]
async fn a_certificate_from_another_ca_fails_the_handshake() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (_, bundle) = setup(&cp).await;
    // The same identity, signed by someone else.
    let (cert, key) = foreign_identity(&cp.env().env_id.to_string(), &bundle.dp_id);
    let forged = DpClient::new(cp.dp_addr(), &cert, &key, &bundle.ca_pem);
    let r = forged
        .try_send(
            "POST",
            "/dp/heartbeat",
            Some(bare_beat(&bundle.dp_id, &Uuid::new_v4().to_string())),
        )
        .await;
    assert!(r.is_err(), "a foreign certificate got an answer: {r:?}");
    let r = forged
        .try_send("GET", "/dp/store/v1/mcp_oauth_token/k-7c1", None)
        .await;
    assert!(r.is_err(), "a foreign certificate got an answer: {r:?}");
    assert_eq!(instance_rows(&cp).await, 0);

    // The control plane's own certificate for that identity is served.
    let r = cp
        .dp_client(&bundle)
        .heartbeat(bare_beat(&bundle.dp_id, &Uuid::new_v4().to_string()))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
}

#[tokio::test]
async fn a_certificate_for_another_environment_is_403() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (admin, _) = setup(&cp).await;
    // This control plane's CA, signing for another environment: the
    // certificate chains, is listed, and names another env.
    let other = EnvSettings {
        env_id: Uuid::new_v4(),
        etcd_prefix: cp.env().etcd_prefix.clone(),
    };
    let pki = Pki::load_or_init(cp.db(), cp.master_key(), &other)
        .await
        .expect("the stored CA");
    assert_eq!(pki.ca_pem(), cp.state().pki.ca_pem(), "not the same CA");
    let dp_id = Uuid::new_v4();
    let issued = pki.issue_client(dp_id, 30).expect("a certificate");
    sqlx::query(
        "INSERT INTO dp_credentials \
         (dp_id, name, serial, fingerprint_sha256, not_before, not_after) \
         VALUES ($1, 'gw-other-env-7c1', $2, $3, $4, $5)",
    )
    .bind(dp_id)
    .bind(&issued.serial)
    .bind(&issued.fingerprint_sha256)
    .bind(issued.not_before)
    .bind(issued.not_after)
    .execute(cp.db().pool())
    .await
    .expect("list the credential");

    let gw = DpClient::new(
        cp.dp_addr(),
        &issued.cert_pem,
        &issued.key_pem,
        cp.state().pki.ca_pem(),
    );
    let r = gw
        .heartbeat(bare_beat(&dp_id.to_string(), &Uuid::new_v4().to_string()))
        .await;
    assert_eq!(r.status, 403, "{r:?}");
    assert_eq!(r.body["code"], "forbidden");
    let r = gw.send("GET", "/dp/store/v1/mcp_oauth_token/k", None).await;
    assert_eq!(r.status, 403, "{r:?}");
    assert!(instances(&cp, &admin).await.is_empty());
}

#[tokio::test]
async fn neither_listener_serves_the_others_paths() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (admin, bundle) = setup(&cp).await;
    let gw = cp.dp_client(&bundle);
    for (method, path) in [
        ("GET", "/healthz"),
        ("GET", "/admin/session"),
        ("POST", "/admin/login"),
        ("GET", "/admin/gateways/instances"),
        ("GET", "/admin/environment"),
    ] {
        let body = (method == "POST")
            .then(|| json!({"email": "nobody@ops.test", "password": "pw-7c1-long-enough"}));
        let r = gw.send(method, path, body).await;
        assert_eq!(r.status, 404, "gateway listener {method} {path}: {r:?}");
    }
    for (method, path) in [
        ("POST", "/dp/heartbeat"),
        ("GET", "/dp/store/v1/mcp_oauth_token/k-7c1"),
        ("PUT", "/dp/store/v1/mcp_oauth_token/k-7c1"),
    ] {
        let body = (method != "GET").then(|| bare_beat(&bundle.dp_id, "i-7c1"));
        let r = cp.send(method, path, Some(admin.cookie()), body).await;
        assert_eq!(r.status, 404, "browser listener {method} {path}: {r:?}");
    }
    assert_eq!(instance_rows(&cp).await, 0);
}

#[tokio::test]
async fn the_dp_store_answers_404() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (_, bundle) = setup(&cp).await;
    let gw = cp.dp_client(&bundle);
    let value = json!({"value": "dG9rZW4tN2Mx", "ttl_seconds": 60, "labels": {"subject": "s-7c1"}});
    for (method, path, body) in [
        ("GET", "/dp/store/v1/mcp_oauth_token/k-7c1", None),
        (
            "PUT",
            "/dp/store/v1/mcp_oauth_token/k-7c1",
            Some(value.clone()),
        ),
        ("DELETE", "/dp/store/v1/mcp_oauth_token/k-7c1", None),
        ("POST", "/dp/store/v1/mcp_oauth_client/k-7c1", Some(value)),
        ("PATCH", "/dp/store/v1/a/b/c", None),
    ] {
        let r = gw.send(method, path, body).await;
        assert_eq!(r.status, 404, "{method} {path}: {r:?}");
        assert_eq!(r.body, json!({}), "{method} {path}");
    }
}

#[tokio::test]
async fn an_instance_not_heard_from_for_the_stale_window_is_offline() {
    let Some(cp) = TestCp::start_with(|c| c.instance_stale_secs = 30).await else {
        return;
    };
    let (admin, bundle) = setup(&cp).await;
    let gw = cp.dp_client(&bundle);
    let (quiet, recent, fresh) = (
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
    );
    for instance in [&quiet, &recent, &fresh] {
        assert_eq!(
            gw.heartbeat(bare_beat(&bundle.dp_id, instance))
                .await
                .status,
            200
        );
    }
    for (instance, ago) in [(&quiet, 31), (&recent, 20)] {
        sqlx::query(
            "UPDATE dp_instances SET last_seen_at = now() - make_interval(secs => $2) \
             WHERE instance_id = $1",
        )
        .bind(instance)
        .bind(f64::from(ago))
        .execute(cp.db().pool())
        .await
        .expect("age the instance");
    }
    let items = instances(&cp, &admin).await;
    let online = |id: &str| {
        items
            .iter()
            .find(|i| i["instance_id"] == id)
            .unwrap_or_else(|| panic!("{id} in {items:?}"))["online"]
            .clone()
    };
    assert_eq!(online(&quiet), false, "{items:?}");
    assert_eq!(online(&recent), true, "{items:?}");
    assert_eq!(online(&fresh), true, "{items:?}");

    // Hearing from it again brings it back.
    assert_eq!(
        gw.heartbeat(bare_beat(&bundle.dp_id, &quiet)).await.status,
        200
    );
    let items = instances(&cp, &admin).await;
    assert!(items.iter().all(|i| i["online"] == true), "{items:?}");
}

#[tokio::test]
async fn the_credential_cache_admits_only_a_live_matching_credential() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (admin, live) = setup(&cp).await;
    let revoked = cp.issue_bundle(&admin, "gw-dp-7c2").await;
    let expired = cp.issue_bundle(&admin, "gw-dp-7c3").await;
    let r = cp
        .send(
            "POST",
            &format!("/admin/gateways/credentials/{}/revoke", revoked.dp_id),
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    sqlx::query(
        "UPDATE dp_credentials SET not_after = now() - interval '1 second' WHERE dp_id = $1::uuid",
    )
    .bind(&expired.dp_id)
    .execute(cp.db().pool())
    .await
    .expect("expire a credential");
    let fingerprint = |b: &Bundle| {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(der_of(&b.cert_pem)))
    };

    let cache = CredentialCache::load(cp.db()).await.expect("load");
    assert!(cache.is_valid(&live.dp_id, &fingerprint(&live)));
    // Another certificate's fingerprint, under the live credential's id.
    assert!(!cache.is_valid(&live.dp_id, &fingerprint(&revoked)));
    assert!(!cache.is_valid(&revoked.dp_id, &fingerprint(&revoked)));
    assert!(!cache.is_valid(&expired.dp_id, &fingerprint(&expired)));
    assert!(!cache.is_valid(&Uuid::new_v4().to_string(), &fingerprint(&live)));
    assert!(!cache.is_valid("not-a-uuid", &fingerprint(&live)));
}

#[tokio::test]
async fn a_credential_this_instance_has_not_heard_of_yet_is_accepted_at_once() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (admin, _) = setup(&cp).await;
    // Listed without a notification, as when it reaches this instance
    // before its notification does (or the notification is lost): only the
    // next full refresh, seconds away, would tell the cache.
    let dp_id = Uuid::new_v4();
    let issued = cp
        .state()
        .pki
        .issue_client(dp_id, 30)
        .expect("a certificate");
    sqlx::query(
        "INSERT INTO dp_credentials \
         (dp_id, name, serial, fingerprint_sha256, not_before, not_after) \
         VALUES ($1, 'gw-unheard-7c1', $2, $3, $4, $5)",
    )
    .bind(dp_id)
    .bind(&issued.serial)
    .bind(&issued.fingerprint_sha256)
    .bind(issued.not_before)
    .bind(issued.not_after)
    .execute(cp.db().pool())
    .await
    .expect("list the credential");
    let gw = DpClient::new(
        cp.dp_addr(),
        &issued.cert_pem,
        &issued.key_pem,
        cp.state().pki.ca_pem(),
    );
    let r = gw
        .heartbeat(bare_beat(&dp_id.to_string(), &Uuid::new_v4().to_string()))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        instances(&cp, &admin).await[0]["credential_name"],
        "gw-unheard-7c1"
    );
}

#[tokio::test]
async fn housekeeping_removes_expired_sessions_and_instances_gone_a_week() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (_, bundle) = setup(&cp).await;
    let gw = cp.dp_client(&bundle);
    let (gone, away) = (Uuid::new_v4().to_string(), Uuid::new_v4().to_string());
    for (instance, days) in [(&gone, 8), (&away, 6)] {
        assert_eq!(
            gw.heartbeat(bare_beat(&bundle.dp_id, instance))
                .await
                .status,
            200
        );
        sqlx::query(
            "UPDATE dp_instances SET last_seen_at = now() - make_interval(days => $2) \
             WHERE instance_id = $1",
        )
        .bind(instance)
        .bind(days)
        .execute(cp.db().pool())
        .await
        .expect("age the instance");
    }
    let op = cp.seed_operator(OperatorRole::Viewer).await;
    for (token, expires) in [(b"expired-7c1", "-1 minute"), (b"current-7c1", "1 hour")] {
        sqlx::query(
            "INSERT INTO sessions (token_sha256, principal_kind, principal_id, expires_at) \
             VALUES ($1, 'operator', $2, now() + $3::interval)",
        )
        .bind(token.as_slice())
        .bind(op.id)
        .bind(expires)
        .execute(cp.db().pool())
        .await
        .expect("seed a session");
    }

    housekeeping::run_once(cp.db())
        .await
        .expect("a housekeeping pass");

    let left: Vec<String> =
        sqlx::query_scalar("SELECT instance_id FROM dp_instances ORDER BY instance_id")
            .fetch_all(cp.db().pool())
            .await
            .expect("read the instances");
    assert_eq!(left, vec![away.clone()]);
    let sessions: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT token_sha256 FROM sessions WHERE principal_id = $1")
            .bind(op.id)
            .fetch_all(cp.db().pool())
            .await
            .expect("read the sessions");
    assert_eq!(sessions, vec![b"current-7c1".to_vec()]);
}

#[tokio::test]
async fn without_an_etcd_front_the_gateway_listener_offers_only_http_1_1() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (_, bundle) = setup(&cp).await;
    let tls = tls_connect(cp.dp_addr(), pems(&bundle), &[b"h2", b"http/1.1"])
        .await
        .expect("the mTLS handshake");
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
}

/// A router answering `GET /who` with `<name> <dp_id>`, the dp_id from the
/// connection's identity.
fn echo(name: &'static str) -> Router {
    Router::new().route(
        "/who",
        get(move |Extension(id): Extension<ConnIdentity>| async move {
            format!("{name} {}", id.dp_id)
        }),
    )
}

async fn text_of(response: hyper::Response<hyper::body::Incoming>) -> String {
    let body = response
        .into_body()
        .collect()
        .await
        .expect("read the body")
        .to_bytes();
    String::from_utf8(body.to_vec()).expect("UTF-8")
}

#[tokio::test]
async fn connections_are_dispatched_by_alpn_and_drained_on_shutdown() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (_, bundle) = setup(&cp).await;
    let tls = listener::server_tls(&cp.state().pki, &["localhost".to_string()], true)
        .expect("the listener's TLS");
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = tcp.local_addr().expect("an address");
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let served = tokio::spawn(listener::serve_dp(
        tcp,
        tls,
        echo("rest"),
        Some(echo("grpc")),
        stopped,
    ));
    let request = || {
        hyper::Request::builder()
            .uri("https://localhost/who")
            .body(Full::new(Bytes::new()))
            .expect("a request")
    };

    let h1 = tls_connect(addr, pems(&bundle), &[b"http/1.1"])
        .await
        .expect("an http/1.1 connection");
    let (mut h1, conn) = hyper::client::conn::http1::handshake(TokioIo::new(h1))
        .await
        .expect("HTTP/1.1");
    let h1_conn = tokio::spawn(conn);
    let answer = h1.send_request(request()).await.expect("an answer");
    assert_eq!(text_of(answer).await, format!("rest {}", bundle.dp_id));

    let h2 = tls_connect(addr, pems(&bundle), &[b"h2"])
        .await
        .expect("an h2 connection");
    assert_eq!(h2.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    let (mut h2, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(h2))
            .await
            .expect("HTTP/2");
    let h2_conn = tokio::spawn(conn);
    let answer = h2.send_request(request()).await.expect("an answer");
    assert_eq!(text_of(answer).await, format!("grpc {}", bundle.dp_id));

    // Both connections are idle and open; shutting down closes them and
    // the listener returns.
    stop.send(true).expect("signal shutdown");
    tokio::time::timeout(Duration::from_secs(5), served)
        .await
        .expect("the listener stops within 5 s")
        .expect("the listener task");
    for conn in [h1_conn, h2_conn] {
        let closed = tokio::time::timeout(Duration::from_secs(5), conn)
            .await
            .expect("the connection closes within 5 s");
        assert!(closed.is_ok(), "{closed:?}");
    }
}

#[test]
fn version_matching_compares_release_cores_or_dev_shas() {
    assert_eq!(
        version_matches("1.9.0+sha-aaa", "1.9.0+sha-bbb"),
        Some(true)
    );
    assert_eq!(
        version_matches("1.8.2+sha-aaa", "1.9.0+sha-bbb"),
        Some(false)
    );
    assert_eq!(version_matches("dev+sha-abc", "dev+sha-abc"), Some(true));
    assert_eq!(version_matches("dev", "1.9.0+sha-bbb"), None);
    // Beyond the brief's four.
    assert_eq!(version_matches("1.9.0", "1.9.0+sha-bbb"), Some(true));
    assert_eq!(version_matches("dev+sha-abc", "dev+sha-abd"), Some(false));
    assert_eq!(version_matches("dev+sha-abc", "1.9.0+sha-abc"), None);
    assert_eq!(version_matches("dev", "dev"), None);
    assert_eq!(version_matches("", "1.9.0"), None);
    assert_eq!(version_matches("not-a-version", "1.9.0"), None);
    // A release candidate is another release than the final one.
    assert_eq!(
        version_matches("1.9.0-rc.1+sha-aaa", "1.9.0+sha-aaa"),
        Some(false)
    );
    assert_eq!(
        version_matches("1.9.0-rc.1+sha-aaa", "1.9.0-rc.1+sha-bbb"),
        Some(true)
    );
}
