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
use aisix_control_plane::dp::heartbeat::MAX_INSTANCES;
use aisix_control_plane::dp::listener::{ConnLimits, MAX_PENDING_HANDSHAKES};
use aisix_control_plane::dp::{listener, ConnIdentity, CredentialCache};
use aisix_control_plane::gateways::version_matches;
use aisix_control_plane::housekeeping;
use aisix_control_plane::pki::Pki;
use aisix_control_plane::resources::write::RESOURCES_LOCK_KEY;
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
use sqlx::{Connection, PgConnection};
use support::{Bundle, DpClient, OperatorSession, TestCp};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

    /// Whether the server closes the connection within `wait`.
    async fn closed_within(&mut self, wait: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + wait;
        while tokio::time::Instant::now() < deadline {
            if self.sender.is_closed() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
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
    // A control plane built as a release, whatever this test binary is.
    let Some(cp) = TestCp::start_with(|c| c.cp_version = "1.9.0+sha-cp7c1".to_string()).await
    else {
        return;
    };
    let (admin, bundle) = setup(&cp).await;
    let view = cp
        .get_with_cookie("/admin/environment", admin.cookie())
        .await;
    assert_eq!(view.body["cp_version"], "1.9.0+sha-cp7c1", "{view:?}");
    let gw = cp.dp_client(&bundle);
    let mut sent = Vec::new();
    for version in [
        // The same release from another image build.
        "1.9.0+sha-gw7c1",
        "1.8.2+sha-cp7c1",
        // The control plane's own commit, but not a release.
        "dev+sha-cp7c1",
        "not-a-version-7c1",
    ] {
        let instance = Uuid::new_v4().to_string();
        let mut beat = bare_beat(&bundle.dp_id, &instance);
        beat["version"] = json!(version);
        assert_eq!(gw.heartbeat(beat).await.status, 200, "{version}");
        sent.push((version, instance));
    }

    let items = instances(&cp, &admin).await;
    let matches_of = |id: &str| {
        items
            .iter()
            .find(|i| i["instance_id"] == id)
            .unwrap_or_else(|| panic!("{id} in {items:?}"))["version_matches_cp"]
            .clone()
    };
    let expected = [json!(true), json!(false), Value::Null, Value::Null];
    for ((version, instance), expected) in sent.iter().zip(expected) {
        assert_eq!(matches_of(instance), expected, "gateway {version}");
    }
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
    // One answer, and then the connection: a revoked credential holds no
    // connection open.
    assert!(
        conn.closed_within(Duration::from_secs(1)).await,
        "the connection stayed open after the 403"
    );

    // A new connection with it is refused too, and closed with its answer.
    let gw = cp.dp_client(&bundle);
    let r = gw
        .send("GET", "/dp/store/v1/mcp_oauth_token/k-7c1", None)
        .await;
    assert_eq!(r.status, 403, "{r:?}");
    let closes = r
        .headers
        .get(reqwest::header::CONNECTION)
        .and_then(|v| v.to_str().ok());
    assert_eq!(closes, Some("close"), "{:?}", r.headers);
    // A refused beat records nothing.
    let mut refused_beat = bare_beat(&bundle.dp_id, &instance);
    refused_beat["applied_revision"] = json!(4242);
    let r = gw.heartbeat(refused_beat).await;
    assert_eq!(r.status, 403, "{r:?}");
    let applied: i64 = sqlx::query_scalar("SELECT applied_revision FROM dp_instances")
        .fetch_one(cp.db().pool())
        .await
        .expect("read the instance");
    assert_ne!(applied, 4242);
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
    // The handshake completes, and the first request is told why.
    let pems = (
        issued.cert_pem.as_str(),
        issued.key_pem.as_str(),
        cp.state().pki.ca_pem(),
    );
    let tls = tls_connect(cp.dp_addr(), pems, &[b"http/1.1"])
        .await
        .expect("the handshake");
    refused_on_http_1_1(tls).await.assert_told_and_closed();
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
    // The control plane's own listener has one now: a listener without.
    let (addr, _stop) = serve_echo(&cp, ConnLimits::default()).await;
    let tls = tls_connect(addr, pems(&bundle), &[b"h2", b"http/1.1"])
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

/// [`echo`] for h2: gRPC routes with one service, `who`, answering every
/// path under `/who/` with `grpc <dp_id>`, the dp_id from the connection's
/// identity.
fn grpc_echo() -> listener::GrpcRoutes {
    #[derive(Clone)]
    struct Who;

    impl tonic::server::NamedService for Who {
        const NAME: &'static str = "who";
    }

    impl tower::Service<hyper::Request<tonic::body::BoxBody>> for Who {
        type Response = hyper::Response<tonic::body::BoxBody>;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(
            &mut self,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: hyper::Request<tonic::body::BoxBody>) -> Self::Future {
            let dp_id = request
                .extensions()
                .get::<ConnIdentity>()
                .map_or_else(|| "none".to_string(), |id| id.dp_id.clone());
            let body = tonic::body::boxed(Full::new(Bytes::from(format!("grpc {dp_id}"))));
            std::future::ready(Ok(hyper::Response::new(body)))
        }
    }

    tonic::service::Routes::new(Who)
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
        Some(grpc_echo()),
        cp.admission(),
        ConnLimits::default(),
        stopped,
    ));
    let request = |path: &str| {
        hyper::Request::builder()
            .uri(format!("https://localhost{path}"))
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
    let answer = h1.send_request(request("/who")).await.expect("an answer");
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
    let answer = h2.send_request(request("/who/x")).await.expect("an answer");
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

/// Whether the listener closes `tls` within `wait`: end of stream or an
/// error, past whatever it sent first (an h2 server's settings).
async fn tls_closed_within(
    tls: &mut tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
    wait: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    let mut buf = [0u8; 1024];
    loop {
        match tokio::time::timeout_at(deadline, tls.read(&mut buf)).await {
            Err(_) => return false,
            Ok(Ok(0) | Err(_)) => return true,
            Ok(Ok(_)) => {}
        }
    }
}

/// What a refused credential's HTTP/1.1 connection answers its first
/// request, and whether the connection is closed within a second after.
#[derive(Debug)]
struct Refusal {
    status: u16,
    code: Value,
    connection: Option<String>,
    closed: bool,
}

impl Refusal {
    /// The gateway is told why, `403 forbidden` with `Connection: close`,
    /// and the connection ends with that answer.
    fn assert_told_and_closed(&self) {
        assert_eq!(
            (
                self.status,
                &self.code,
                self.connection.as_deref(),
                self.closed
            ),
            (403, &json!("forbidden"), Some("close"), true),
            "{self:?}"
        );
    }
}

/// `GET /dp/store/v1/ns/k-7c1` over `tls`, and what came back.
async fn refused_on_http_1_1(
    tls: tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
) -> Refusal {
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .expect("an HTTP/1.1 connection");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let request = hyper::Request::builder()
        .uri("/dp/store/v1/ns/k-7c1")
        .header("host", "localhost")
        .body(Full::new(Bytes::new()))
        .expect("a request");
    let response = sender.send_request(request).await.expect("an answer");
    let status = response.status().as_u16();
    let connection = response
        .headers()
        .get(hyper::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("read the body")
        .to_bytes();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while !sender.is_closed() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Refusal {
        status,
        code: body["code"].clone(),
        connection,
        closed: sender.is_closed(),
    }
}

/// A credential revoked before it expires still completes its TLS
/// handshake, and is then told why, so that its gateway can tell a refusal
/// from an outage: `403 forbidden` with `Connection: close` on HTTP/1.1,
/// `PermissionDenied` and then GOAWAY on h2. Either way the connection then
/// closes, and one that sends nothing is closed within the handshake
/// timeout: it holds its slot no longer than a peer in its handshake. A
/// listed credential's idle connection stays open.
#[tokio::test]
async fn a_refused_credential_is_told_why_and_closed_within_a_handshakes_time() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let (admin, bundle) = setup(&cp).await;
    let addr = cp.dp_addr();
    let protocols: [&[u8]; 2] = [b"http/1.1", b"h2"];
    for alpn in protocols {
        let mut tls = tls_connect(addr, pems(&bundle), &[alpn])
            .await
            .expect("the handshake");
        assert!(
            !tls_closed_within(&mut tls, Duration::from_secs(1)).await,
            "{}: a listed credential's idle connection was closed",
            String::from_utf8_lossy(alpn)
        );
    }

    let r = cp
        .send(
            "POST",
            &format!("/admin/gateways/credentials/{}/revoke", bundle.dp_id),
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    // Known once a new connection is refused; until then it is served.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let refusal = loop {
        let tls = tls_connect(addr, pems(&bundle), &[b"http/1.1"])
            .await
            .expect("the handshake");
        let answer = refused_on_http_1_1(tls).await;
        if answer.status != 404 {
            break answer;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "still served 5 s after the revoke"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    refusal.assert_told_and_closed();

    // h2: the first RPC is refused with `PermissionDenied`, and the
    // connection goes away.
    let tls = tls_connect(addr, pems(&bundle), &[b"h2"])
        .await
        .expect("the handshake");
    let (client, connection) = h2::client::handshake(tls).await.expect("HTTP/2");
    let connection = tokio::spawn(connection);
    let mut client = client.ready().await.expect("ready");
    let request = hyper::Request::builder()
        .method("POST")
        .uri("https://localhost/etcdserverpb.KV/Range")
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(())
        .expect("a request");
    let (answer, mut body) = client.send_request(request, false).expect("send");
    // An empty RangeRequest, framed: the credential is judged first.
    body.send_data(Bytes::from_static(&[0, 0, 0, 0, 0]), true)
        .expect("the message");
    let answer = answer.await.expect("an answer");
    assert_eq!(
        answer.headers().get("grpc-status").map(|v| v.as_bytes()),
        Some(&b"7"[..]),
        "{answer:?}"
    );
    tokio::time::timeout(Duration::from_secs(2), connection)
        .await
        .expect("the h2 connection ends within 2 s of the refusal")
        .expect("the connection task")
        .expect("ended with GOAWAY, not an error");

    // A refused peer that sends nothing is closed within the handshake
    // timeout, on either protocol.
    let mut idle = Vec::new();
    for alpn in protocols {
        idle.push(
            tls_connect(addr, pems(&bundle), &[alpn])
                .await
                .expect("the handshake"),
        );
    }
    let bound = listener::HANDSHAKE_TIMEOUT + Duration::from_secs(1);
    let (mut h1, mut h2) = (idle.remove(0), idle.remove(0));
    let (h1_closed, h2_closed) = tokio::join!(
        tls_closed_within(&mut h1, bound),
        tls_closed_within(&mut h2, bound)
    );
    assert!(h1_closed, "a silent HTTP/1.1 connection outlived {bound:?}");
    assert!(h2_closed, "a silent h2 connection outlived {bound:?}");
}

/// On shutdown an HTTP/1.1 request still in flight, its body stopped
/// arriving, is given the drain deadline and then dropped, so the listener
/// returns and the process can exit on SIGTERM.
#[tokio::test]
async fn shutdown_drops_an_http_1_1_request_still_in_flight_at_the_drain_deadline() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (_, bundle) = setup(&cp).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let rest = Router::new().route(
        "/slow",
        axum::routing::post({
            let entered = entered.clone();
            move |request: axum::extract::Request| async move {
                entered.notify_one();
                let _ = request.into_body().collect().await;
                "done"
            }
        }),
    );
    let tls = listener::server_tls(&cp.state().pki, &["localhost".to_string()], false)
        .expect("the listener's TLS");
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = tcp.local_addr().expect("an address");
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let drain = Duration::from_millis(300);
    let served = tokio::spawn(listener::serve_dp(
        tcp,
        tls,
        rest,
        None,
        cp.admission(),
        ConnLimits {
            drain_deadline: drain,
            ..ConnLimits::default()
        },
        stopped,
    ));
    let mut tls = tls_connect(addr, pems(&bundle), &[b"http/1.1"])
        .await
        .expect("the handshake");
    tls.write_all(b"POST /slow HTTP/1.1\r\nhost: localhost\r\ncontent-length: 100\r\n\r\npartial")
        .await
        .expect("send part of a request");
    tls.flush().await.expect("flush");
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("the request reaches its handler");

    let stopping = tokio::time::Instant::now();
    stop.send(true).expect("signal shutdown");
    tokio::time::timeout(Duration::from_secs(5), served)
        .await
        .expect("the listener stops while a request is stuck")
        .expect("the listener task");
    assert!(
        stopping.elapsed() >= drain,
        "the request was dropped before its drain deadline"
    );
    assert!(tls_closed_within(&mut tls, Duration::from_secs(1)).await);
}

/// The listener's accepted sockets have TCP keepalive: an idle connection
/// of the server's side runs the keepalive timer (`tr` 2 in
/// `/proc/net/tcp`), so a gateway that vanished without closing is noticed.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_accepted_connection_has_tcp_keepalive() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (_, bundle) = setup(&cp).await;
    let (addr, _stop) = serve_echo(&cp, ConnLimits::default()).await;
    let tls = tls_connect(addr, pems(&bundle), &[b"http/1.1"])
        .await
        .expect("the handshake");
    let client = tls.get_ref().0.local_addr().expect("the client's address");
    // The server's end: local is the listener's port, remote the client's.
    let hex = |port: u16| format!("{port:04X}");
    let (local, remote) = (hex(addr.port()), hex(client.port()));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let table = std::fs::read_to_string("/proc/net/tcp").expect("read /proc/net/tcp");
        let timer = table.lines().find_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let port = |field: &str| field.rsplit(':').next().map(str::to_string);
            (fields.len() > 5
                && port(fields[1]).as_deref() == Some(local.as_str())
                && port(fields[2]).as_deref() == Some(remote.as_str()))
            .then(|| fields[5].split(':').next().unwrap_or("").to_string())
        });
        if timer.as_deref() == Some("02") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the server's end of the connection runs no keepalive timer: {timer:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(tls);
}

/// A `serve_dp` on the control plane's own TLS configuration, `echo("rest")`
/// behind it, with `limits`: its address, and the sender that stops it.
async fn serve_echo(
    cp: &TestCp,
    limits: ConnLimits,
) -> (std::net::SocketAddr, tokio::sync::watch::Sender<bool>) {
    let tls = listener::server_tls(&cp.state().pki, &["localhost".to_string()], false)
        .expect("the listener's TLS");
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = tcp.local_addr().expect("an address");
    let (stop, stopped) = tokio::sync::watch::channel(false);
    tokio::spawn(listener::serve_dp(
        tcp,
        tls,
        echo("rest"),
        None,
        cp.admission(),
        limits,
        stopped,
    ));
    (addr, stop)
}

/// Whether the listener closes a connection that has sent nothing, within
/// a second: end of stream, or a reset.
async fn closed_within_a_second(tcp: &mut tokio::net::TcpStream) -> bool {
    let mut buf = [0u8; 16];
    matches!(
        tokio::time::timeout(Duration::from_secs(1), tcp.read(&mut buf)).await,
        Ok(Ok(0) | Err(_))
    )
}

/// `GET /who` over a new mTLS connection as `bundle`, or `None` when the
/// connection is refused.
async fn who(addr: std::net::SocketAddr, bundle: &Bundle) -> Option<String> {
    let tls = tls_connect(addr, pems(bundle), &[b"http/1.1"]).await.ok()?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .ok()?;
    tokio::spawn(conn);
    let request = hyper::Request::builder()
        .uri("/who")
        .header("host", "localhost")
        .body(Full::new(Bytes::new()))
        .expect("a request");
    let answer = sender.send_request(request).await.ok()?;
    Some(text_of(answer).await)
}

#[tokio::test]
async fn a_peer_cannot_hold_more_than_the_pending_handshake_cap() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (_, bundle) = setup(&cp).await;
    // A cap of its own, and a handshake timeout no runner is slow enough to
    // reach, so neither the open-file limit nor the runner's speed decides
    // the outcome. The default caps are pinned below.
    let pending = 64;
    let limits = ConnLimits {
        pending_handshakes: pending,
        handshake_timeout: Duration::from_secs(60),
        ..ConnLimits::default()
    };
    let (addr, _stop) = serve_echo(&cp, limits).await;
    // Connections that never start a handshake, up to the cap.
    let mut silent = Vec::new();
    for _ in 0..pending {
        silent.push(tokio::net::TcpStream::connect(addr).await.expect("connect"));
    }
    // One more is closed at once, unread, and so is a gateway's.
    let mut extra = tokio::net::TcpStream::connect(addr).await.expect("connect");
    assert!(
        closed_within_a_second(&mut extra).await,
        "a connection over the cap was held"
    );
    assert_eq!(who(addr, &bundle).await, None, "served over the cap");
    // The rest of the process is unaffected.
    let health = cp.send("GET", "/healthz", None, None).await;
    assert_eq!(health.status, 200, "{health:?}");

    // A slot frees as soon as a handshake ends, here by its peer leaving.
    drop(silent.pop());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(answer) = who(addr, &bundle).await {
            assert_eq!(answer, format!("rest {}", bundle.dp_id));
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no slot freed after a handshake ended"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn established_connections_are_capped_too() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (_, bundle) = setup(&cp).await;
    let limits = ConnLimits {
        connections: 2,
        ..ConnLimits::default()
    };
    let (addr, _stop) = serve_echo(&cp, limits).await;
    // Two gateways connected and idle: a revoked certificate gets this far
    // too, so the handshake cap alone does not bound them.
    let mut held = Vec::new();
    for _ in 0..2 {
        let tls = tls_connect(addr, pems(&bundle), &[b"http/1.1"])
            .await
            .expect("the mTLS handshake");
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .expect("HTTP/1.1");
        let task = tokio::spawn(conn);
        let request = hyper::Request::builder()
            .uri("/who")
            .header("host", "localhost")
            .body(Full::new(Bytes::new()))
            .expect("a request");
        let answer = sender.send_request(request).await.expect("an answer");
        assert_eq!(text_of(answer).await, format!("rest {}", bundle.dp_id));
        held.push((sender, task));
    }
    let mut third = tokio::net::TcpStream::connect(addr).await.expect("connect");
    assert!(
        closed_within_a_second(&mut third).await,
        "a connection over the cap was held"
    );
    assert_eq!(who(addr, &bundle).await, None, "served over the cap");

    // One leaves, and its slot is taken again.
    let (sender, task) = held.pop().expect("a held connection");
    drop(sender);
    let _ = task.await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while who(addr, &bundle).await.is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no slot freed after a connection closed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_handshake_slot_frees_when_its_handshake_completes() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (_, bundle) = setup(&cp).await;
    let limits = ConnLimits {
        pending_handshakes: 1,
        ..ConnLimits::default()
    };
    let (addr, _stop) = serve_echo(&cp, limits).await;
    // One gateway connected and staying connected.
    let tls = tls_connect(addr, pems(&bundle), &[b"http/1.1"])
        .await
        .expect("the mTLS handshake");
    let (mut first, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .expect("HTTP/1.1");
    tokio::spawn(conn);
    let request = hyper::Request::builder()
        .uri("/who")
        .header("host", "localhost")
        .body(Full::new(Bytes::new()))
        .expect("a request");
    let answer = first.send_request(request).await.expect("an answer");
    assert_eq!(text_of(answer).await, format!("rest {}", bundle.dp_id));
    // Its handshake is over, so the one slot takes another.
    assert_eq!(
        who(addr, &bundle).await,
        Some(format!("rest {}", bundle.dp_id)),
        "the handshake slot is held past the handshake"
    );
    assert!(!first.is_closed());
}

#[tokio::test]
async fn a_connection_without_a_client_certificate_gets_no_answer() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (_, bundle) = setup(&cp).await;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(der_of(&bundle.ca_pem)))
        .expect("the CA as a root");
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let tcp = tokio::net::TcpStream::connect(cp.dp_addr())
        .await
        .expect("connect");
    let connected = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").expect("a name"), tcp)
        .await;
    // TLS 1.3 lets the client finish its side first; the refusal then
    // arrives as an alert instead of an answer.
    if let Ok(tls) = connected {
        let answered = async {
            let (mut sender, conn) =
                hyper::client::conn::http1::handshake(TokioIo::new(tls)).await?;
            tokio::spawn(conn);
            let request = hyper::Request::builder()
                .method("POST")
                .uri("/dp/heartbeat")
                .header("host", "localhost")
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(
                    bare_beat(&bundle.dp_id, "anonymous-7c1").to_string(),
                )))
                .expect("a request");
            sender.send_request(request).await
        };
        let answered = tokio::time::timeout(Duration::from_secs(5), answered)
            .await
            .expect("refused within 5 s");
        assert!(
            answered.is_err(),
            "answered without a certificate: {answered:?}"
        );
    }
    assert_eq!(instance_rows(&cp).await, 0);
}

#[tokio::test]
async fn a_credential_flooding_heartbeats_is_throttled() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (admin, bundle) = setup(&cp).await;
    let instance = Uuid::new_v4().to_string();
    let mut conn = DpConn::open(&cp, &bundle).await;
    let mut answers = Vec::new();
    for revision in 0..400 {
        let mut beat = bare_beat(&bundle.dp_id, &instance);
        beat["applied_revision"] = json!(revision);
        answers.push(conn.send("POST", "/dp/heartbeat", Some(&beat)).await);
    }
    // A burst well past any real fleet's goes through; a flood does not.
    assert!(
        answers[..100].iter().all(|(status, _)| *status == 200),
        "{:?}",
        &answers[..100]
    );
    let refused = answers
        .iter()
        .find(|(status, _)| *status != 200)
        .expect("a flood of 400 beats was never throttled");
    assert_eq!(refused.0, 429, "{refused:?}");
    assert_eq!(refused.1["code"], "busy", "{refused:?}");
    // Per credential: another one is not held back.
    let other = cp.issue_bundle(&admin, "gw-dp-7c2").await;
    let r = cp
        .dp_client(&other)
        .heartbeat(bare_beat(&other.dp_id, &Uuid::new_v4().to_string()))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
}

/// Seeds `count` instances of `dp_id` heard from just now, as that many
/// gateways beating would leave them.
async fn seed_online_instances(cp: &TestCp, dp_id: &str, count: i64) {
    sqlx::query(
        "INSERT INTO dp_instances (dp_id, instance_id) \
         SELECT $1::uuid, 'online-7c1-' || g FROM generate_series(1, $2) g",
    )
    .bind(dp_id)
    .bind(count)
    .execute(cp.db().pool())
    .await
    .expect("seed instances");
}

/// `dp_id`'s instance rows, and when it last hit its instance cap.
async fn instance_cap_state(
    cp: &TestCp,
    dp_id: &str,
) -> (i64, Option<chrono::DateTime<chrono::Utc>>) {
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM dp_instances WHERE dp_id = $1::uuid), \
         instance_cap_hit_at FROM dp_credentials WHERE dp_id = $1::uuid",
    )
    .bind(dp_id)
    .fetch_one(cp.db().pool())
    .await
    .expect("read the credential")
}

#[tokio::test]
async fn at_the_instance_cap_a_new_instance_takes_the_place_of_the_longest_silent_one() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (_, bundle) = setup(&cp).await;
    let gw = cp.dp_client(&bundle);
    // The cap reached: all heard from just now but three, which crashed a
    // while ago and are offline.
    seed_online_instances(&cp, &bundle.dp_id, MAX_INSTANCES - 3).await;
    for (instance, ago) in [
        ("crashed-5m-7c1", "5 minutes"),
        ("crashed-20m-7c1", "20 minutes"),
        ("crashed-10m-7c1", "10 minutes"),
    ] {
        sqlx::query(
            "INSERT INTO dp_instances (dp_id, instance_id, last_seen_at) \
             VALUES ($1::uuid, $2, now() - $3::interval)",
        )
        .bind(&bundle.dp_id)
        .bind(instance)
        .bind(ago)
        .execute(cp.db().pool())
        .await
        .expect("seed a crashed instance");
    }
    assert_eq!(
        instance_cap_state(&cp, &bundle.dp_id).await.0,
        MAX_INSTANCES
    );

    let ids = |cp: &TestCp| {
        let pool = cp.db().pool().clone();
        async move {
            sqlx::query_scalar::<_, String>("SELECT instance_id FROM dp_instances")
                .fetch_all(&pool)
                .await
                .expect("read the instances")
        }
    };
    // Each new live instance takes the place of the longest silent one.
    for (n, gone) in ["crashed-20m-7c1", "crashed-10m-7c1"]
        .into_iter()
        .enumerate()
    {
        let live = format!("live-{n}-7c1");
        let r = gw.heartbeat(bare_beat(&bundle.dp_id, &live)).await;
        assert_eq!(r.status, 200, "{live}: {r:?}");
        let left = ids(&cp).await;
        assert!(left.contains(&live), "{live} was not stored");
        assert!(!left.contains(&gone.to_string()), "{gone} is still there");
        assert!(left.contains(&"crashed-5m-7c1".to_string()));
        let (rows, hit) = instance_cap_state(&cp, &bundle.dp_id).await;
        assert_eq!(rows, MAX_INSTANCES, "the rows outgrew the cap");
        assert_eq!(
            hit, None,
            "a cap hit was recorded though nothing was refused"
        );
    }
}

#[tokio::test]
async fn at_the_instance_cap_with_every_instance_online_a_new_one_is_refused_and_recorded() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (_, bundle) = setup(&cp).await;
    let gw = cp.dp_client(&bundle);
    seed_online_instances(&cp, &bundle.dp_id, MAX_INSTANCES).await;

    let over = Uuid::new_v4().to_string();
    let r = gw.heartbeat(bare_beat(&bundle.dp_id, &over)).await;
    assert_eq!(r.status, 429, "{r:?}");
    assert_eq!(r.body["code"], "instance_limit", "{r:?}");
    let (rows, hit) = instance_cap_state(&cp, &bundle.dp_id).await;
    assert_eq!(rows, MAX_INSTANCES, "the refused instance was stored");
    let hit = hit.expect("the refusal is recorded on the credential");
    assert!(
        (chrono::Utc::now() - hit).num_seconds() < 60,
        "recorded at {hit}"
    );
    // Instances already known keep reporting.
    let r = gw.heartbeat(bare_beat(&bundle.dp_id, "online-7c1-1")).await;
    assert_eq!(r.status, 200, "{r:?}");
    // Once one goes offline, the new one takes its place.
    sqlx::query(
        "UPDATE dp_instances SET last_seen_at = now() - interval '2 minutes' \
         WHERE instance_id = 'online-7c1-2'",
    )
    .execute(cp.db().pool())
    .await
    .expect("age an instance");
    let r = gw.heartbeat(bare_beat(&bundle.dp_id, &over)).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        instance_cap_state(&cp, &bundle.dp_id).await.0,
        MAX_INSTANCES
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn new_instances_arriving_together_at_the_cap_get_its_one_place() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (_, bundle) = setup(&cp).await;
    // One place left, and nothing offline to make room.
    seed_online_instances(&cp, &bundle.dp_id, MAX_INSTANCES - 1).await;
    let racers = 24;
    let gw = Arc::new(cp.dp_client(&bundle));
    let barrier = Arc::new(tokio::sync::Barrier::new(racers));
    let beats: Vec<_> = (0..racers)
        .map(|_| {
            let (gw, barrier) = (gw.clone(), barrier.clone());
            let beat = bare_beat(&bundle.dp_id, &Uuid::new_v4().to_string());
            tokio::spawn(async move {
                barrier.wait().await;
                gw.heartbeat(beat).await
            })
        })
        .collect();
    let mut answers = Vec::new();
    for beat in beats {
        answers.push(beat.await.expect("a beat task"));
    }
    let admitted = answers.iter().filter(|r| r.status == 200).count();
    let refused = answers
        .iter()
        .filter(|r| r.status == 429 && r.body["code"] == "instance_limit")
        .count();
    assert_eq!((admitted, refused), (1, racers - 1), "{answers:?}");
    assert_eq!(
        instance_cap_state(&cp, &bundle.dp_id).await.0,
        MAX_INSTANCES
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_first_beats_of_one_instance_arriving_together_at_the_boundary_are_all_admitted() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (_, bundle) = setup(&cp).await;
    seed_online_instances(&cp, &bundle.dp_id, MAX_INSTANCES - 1).await;
    // A gateway retrying its first beat: the same new id, several times
    // at once. It takes the last place once, and none of them is refused.
    let instance = Uuid::new_v4().to_string();
    let gw = Arc::new(cp.dp_client(&bundle));
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let beats: Vec<_> = (0..8)
        .map(|_| {
            let (gw, barrier) = (gw.clone(), barrier.clone());
            let beat = bare_beat(&bundle.dp_id, &instance);
            tokio::spawn(async move {
                barrier.wait().await;
                gw.heartbeat(beat).await
            })
        })
        .collect();
    for beat in beats {
        let r = beat.await.expect("a beat task");
        assert_eq!(r.status, 200, "{r:?}");
    }
    let (rows, hit) = instance_cap_state(&cp, &bundle.dp_id).await;
    assert_eq!(rows, MAX_INSTANCES);
    assert_eq!(hit, None);
}

#[tokio::test]
async fn revocation_is_seen_while_the_main_pool_is_exhausted() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (_, bundle) = setup(&cp).await;
    let mut conn = DpConn::open(&cp, &bundle).await;
    let store = "/dp/store/v1/mcp_oauth_token/k-7c1";
    assert_eq!(conn.send("GET", store, None).await.0, 404);

    // Request traffic holds every connection of the main pool.
    let pool = cp.db().pool();
    let mut held = Vec::new();
    for _ in 0..pool.options().get_max_connections() {
        held.push(pool.acquire().await.expect("a pool connection"));
    }
    // Revoked by another instance, as its revoke route would.
    let mut other = PgConnection::connect(cp.database_url())
        .await
        .expect("connect");
    let mut tx = other.begin().await.expect("begin");
    sqlx::query("UPDATE dp_credentials SET revoked_at = now() WHERE dp_id = $1::uuid")
        .bind(&bundle.dp_id)
        .execute(&mut *tx)
        .await
        .expect("revoke");
    sqlx::query("SELECT pg_notify('dp_credentials_changed', $1)")
        .bind(&bundle.dp_id)
        .execute(&mut *tx)
        .await
        .expect("notify");
    tx.commit().await.expect("commit");

    // The store stub reads nothing but the credentials.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (status, _) = conn.send("GET", store, None).await;
        if status == 403 {
            break;
        }
        assert_eq!(status, 404);
        assert!(
            tokio::time::Instant::now() < deadline,
            "the revoke was not seen within 5 s while the main pool was busy"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(held);
}

#[tokio::test]
async fn a_heartbeat_with_every_optional_field_null_is_accepted() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (admin, bundle) = setup(&cp).await;
    let instance = Uuid::new_v4().to_string();
    let mut beat = full_beat(&bundle.dp_id, &instance);
    for field in [
        "hostname",
        "version",
        "uptime_seconds",
        "applied_revision",
        "config_hash",
        "supported_guardrail_kinds",
        "exporter_health",
        "rejected_resources",
        "partially_compatible_resources",
    ] {
        beat[field] = Value::Null;
    }
    let r = cp.dp_client(&bundle).heartbeat(beat).await;
    assert_eq!(r.status, 200, "{r:?}");
    let items = instances(&cp, &admin).await;
    let item = &items[0];
    assert_eq!(item["hostname"], "");
    assert_eq!(item["version"], "");
    assert_eq!(item["uptime_seconds"], 0);
    assert_eq!(item["applied_revision"], 0);
    assert_eq!(item["config_hash"], Value::Null);
    assert_eq!(item["exporter_health"], json!([]));
    assert_eq!(item["rejected_count"], 0);
    assert_eq!(stored_lists(&cp, &instance).await, (json!([]), json!([])));
}

/// A NUL, which neither `text` nor `jsonb` can hold, reaches a heartbeat
/// when an exporter copies a remote error body into `last_error`. The beat
/// is stored with U+FFFD in its place, wherever it is, so the instance stays
/// in the fleet view and before the version gate.
#[tokio::test]
async fn a_heartbeat_carrying_a_nul_is_stored_with_a_replacement_character() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (admin, bundle) = setup(&cp).await;
    let instance = format!("{}\u{0}i", Uuid::new_v4());
    let mut beat = full_beat(&bundle.dp_id, &instance);
    beat["exporter_health"][0]["last_error"] = json!("HTTP 502: a\u{0}b");
    beat["hostname"] = json!("gw\u{0}host");
    beat["config_hash"] = json!("ab\u{0}cd");
    beat["supported_guardrail_kinds"] = json!(["keyword\u{0}"]);
    beat["rejected_resources"][0]["error"] = json!("bad\u{0}row");
    beat["partially_compatible_resources"][0]["field\u{0}"] = json!(1);
    let r = cp.dp_client(&bundle).heartbeat(beat).await;
    assert_eq!(r.status, 200, "{r:?}");

    let stored = instance.replace('\0', "\u{fffd}");
    let items = instances(&cp, &admin).await;
    let item = items
        .iter()
        .find(|i| i["instance_id"] == stored.as_str())
        .unwrap_or_else(|| panic!("the instance is listed: {items:?}"));
    assert_eq!(
        item["exporter_health"][0]["last_error"],
        "HTTP 502: a\u{fffd}b"
    );
    assert_eq!(item["hostname"], "gw\u{fffd}host");
    assert_eq!(item["config_hash"], "ab\u{fffd}cd");
    let (rejected, partial) = stored_lists(&cp, &stored).await;
    assert_eq!(rejected[0]["error"], "bad\u{fffd}row");
    assert_eq!(partial[0]["field\u{fffd}"], 1);
    let kinds: Value = sqlx::query_scalar(
        "SELECT supported_guardrail_kinds FROM dp_instances WHERE instance_id = $1",
    )
    .bind(&stored)
    .fetch_one(cp.db().pool())
    .await
    .expect("read the instance");
    assert_eq!(kinds, json!(["keyword\u{fffd}"]));
}

/// Seeds gateway resource rows `(what, deleted, projected_version, age)`,
/// each at version 3, and returns their ids.
async fn seed_resources(
    cp: &TestCp,
    rows: &[(&'static str, bool, Option<i64>, &str)],
) -> Vec<(&'static str, Uuid)> {
    let mut ids = Vec::new();
    for (what, deleted, projected, age) in rows {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gw_resources (kind, id, doc, version, deleted, projected_version, updated_at) \
             VALUES ('models', $1, '{}', 3, $2, $3, now() - $4::interval)",
        )
        .bind(id)
        .bind(deleted)
        .bind(projected)
        .bind(age)
        .execute(cp.db().pool())
        .await
        .unwrap_or_else(|e| panic!("seed {what}: {e}"));
        ids.push((*what, id));
    }
    ids
}

async fn resource_ids(cp: &TestCp) -> Vec<Uuid> {
    sqlx::query_scalar("SELECT id FROM gw_resources")
        .fetch_all(cp.db().pool())
        .await
        .expect("read the resources")
}

#[tokio::test]
async fn housekeeping_prunes_tombstones_projected_more_than_a_day_ago() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    // The prune, with a window of an hour: rows the control plane's own
    // pass (a day) never touches, so what this call reports is all there is.
    let ids = seed_resources(
        &cp,
        &[
            ("projected-tombstone-2h", true, Some(3), "2 hours"),
            ("projected-tombstone-30m", true, Some(3), "30 minutes"),
            ("unprojected-tombstone-2h", true, Some(2), "2 hours"),
            ("never-projected-tombstone-2h", true, None, "2 hours"),
            ("live-row-2h", false, Some(3), "2 hours"),
        ],
    )
    .await;

    // It waits for the resources lock, as a write would: a tombstone being
    // revived on its id is never pruned under the write.
    let mut writer = PgConnection::connect(cp.database_url())
        .await
        .expect("connect");
    let mut tx = writer.begin().await.expect("begin");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(RESOURCES_LOCK_KEY)
        .execute(&mut *tx)
        .await
        .expect("take the resources lock");
    let db = cp.db().clone();
    let pass = tokio::spawn(async move {
        housekeeping::prune_tombstones(&db, Duration::from_secs(3600)).await
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !pass.is_finished(),
        "pruned while a write held the resources lock"
    );
    tx.commit().await.expect("release the lock");
    let pruned = tokio::time::timeout(Duration::from_secs(5), pass)
        .await
        .expect("the prune ends once the lock is free")
        .expect("the prune task")
        .expect("a prune");
    assert_eq!(pruned, 1);
    let left = resource_ids(&cp).await;
    for (what, id) in &ids {
        assert_eq!(
            left.contains(id),
            *what != "projected-tombstone-2h",
            "{what}"
        );
    }

    // A pass prunes with a day's window. Another pass may run beside it,
    // so what is left is what is checked.
    let ids = seed_resources(
        &cp,
        &[
            ("projected-tombstone-2d", true, Some(3), "2 days"),
            ("projected-tombstone-12h", true, Some(3), "12 hours"),
        ],
    )
    .await;
    housekeeping::run_once(cp.db())
        .await
        .expect("a housekeeping pass");
    let left = resource_ids(&cp).await;
    assert!(!left.contains(&ids[0].1), "{}", ids[0].0);
    assert!(left.contains(&ids[1].1), "{}", ids[1].0);
}

#[tokio::test]
async fn a_control_plane_prunes_as_it_boots() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    // Left behind by an instance that restarted before its next pass.
    let op = cp.seed_operator(OperatorRole::Viewer).await;
    sqlx::query(
        "INSERT INTO sessions (token_sha256, principal_kind, principal_id, expires_at) \
         VALUES ('\\x0b1d'::bytea, 'operator', $1, now() - interval '1 minute')",
    )
    .bind(op.id)
    .execute(cp.db().pool())
    .await
    .expect("seed an expired session");
    let restarted = cp.restart().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE principal_id = $1")
            .bind(op.id)
            .fetch_one(restarted.db().pool())
            .await
            .expect("count sessions");
        if left == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "a restarted control plane did not prune within 5 s"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn boot_raises_the_open_file_limit_to_its_hard_limit() {
    use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};

    let limit = getrlimit(Resource::Nofile);
    let Some(hard) = limit.maximum else {
        // No hard limit: there is nothing to raise to.
        return;
    };
    // Just below the hard limit, so the tests beside this one cannot tell.
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(hard - 1),
            maximum: Some(hard),
        },
    )
    .expect("lower the soft limit");
    let Some(_cp) = TestCp::start().await else {
        return;
    };
    assert_eq!(getrlimit(Resource::Nofile).current, Some(hard));
}

#[test]
fn the_listener_caps_512_handshakes_4096_connections_and_5_s() {
    assert_eq!(MAX_PENDING_HANDSHAKES, 512);
    assert_eq!(
        ConnLimits::default(),
        ConnLimits {
            pending_handshakes: 512,
            connections: 4096,
            handshake_timeout: Duration::from_secs(5),
            drain_deadline: Duration::from_secs(5),
        }
    );
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
