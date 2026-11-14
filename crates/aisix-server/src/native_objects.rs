//! 托管原生对象归属的控制面客户端。

use aisix_core::native_realtime::SignedSipIntakeRequest;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use aisix_core::native_metering::{ReconciliationRequest, TypedCompleteRequest};
use aisix_core::native_objects::{
    BeginRelatedRequest, BeginRequest, BindObservedArtifactsRequest, BindRelatedRequest,
    BindRequest, ClaimRequest, CommitRelationsRequest, CompleteRequest, LeaseRequest,
    LookupRelationsRequest, LookupRequest, NativeReply, ObjectDeletionRequest, OwnedRequest,
    ReserveRelationsRequest, ResolveReservationRequest, TombstoneRequest,
};
use aisix_core::native_realtime::{
    AppendLiveUsageRequest, CheckpointLiveSessionRequest, ClaimLiveSessionsRequest,
    CreateLiveSessionRequest, MintCredentialRequest, RedeemCredentialRequest,
};
use aisix_proxy::native_api::registry::{NativeObjectRegistry, NativeRegistryError};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::heartbeat::MtlsBundle;

// 包含控制面的连接池等待和事务上限，且覆盖响应体读取。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
// 批次恢复记录最多携带 1000 个逐项身份，仍按固定上限读取整份控制面回复。
const MAX_REPLY_BYTES: usize = 8 * 1024 * 1024;
type ReplyFuture<'a> =
    Pin<Box<dyn Future<Output = Result<NativeReply, NativeRegistryError>> + Send + 'a>>;

pub struct NativeObjectsClient {
    http: reqwest::Client,
    base: String,
}

impl NativeObjectsClient {
    pub fn new(origin: &str, mtls: &MtlsBundle) -> anyhow::Result<Self> {
        // 新建独立连接池，复用既有 mTLS、HTTP/1 和禁止重定向的配置。
        Ok(Self::with_client(
            origin,
            crate::heartbeat::build_client(mtls)?,
        ))
    }

    fn with_client(origin: &str, http: reqwest::Client) -> Self {
        Self {
            http,
            base: format!("{}/dp/v1/native-objects", origin.trim_end_matches('/')),
        }
    }

    async fn post<T: Serialize + Sync>(
        &self,
        operation: &'static str,
        body: &T,
    ) -> Result<NativeReply, NativeRegistryError> {
        // 失败不自动重发：上一次提交可能已持久化，调用方持有其操作身份。
        let mut response = self
            .http
            .post(format!("{}/{operation}", self.base))
            .timeout(REQUEST_TIMEOUT)
            .json(body)
            .send()
            .await
            .map_err(|error| {
                tracing::warn!(
                    error = %aisix_gateway::transport_error_message(&error.without_url()),
                    "native object authority is unavailable",
                );
                NativeRegistryError::Unavailable
            })?;
        let status = response.status();
        match status {
            StatusCode::OK
            | StatusCode::NOT_FOUND
            | StatusCode::UNPROCESSABLE_ENTITY
            | StatusCode::CONFLICT => {}
            _ => return Err(NativeRegistryError::Unavailable),
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_REPLY_BYTES as u64)
        {
            return Err(NativeRegistryError::Unavailable);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            aisix_gateway::log_body_read_causes(&error.without_url());
            NativeRegistryError::Unavailable
        })? {
            if chunk.len() > MAX_REPLY_BYTES.saturating_sub(bytes.len()) {
                return Err(NativeRegistryError::Unavailable);
            }
            bytes.extend_from_slice(&chunk);
        }
        if status != StatusCode::OK {
            // 只有已知可判别的状态冲突对调用方可见；账号更换不得触发重路由。
            let failure: AuthorityFailure =
                serde_json::from_slice(&bytes).map_err(|_| NativeRegistryError::Unavailable)?;
            return Err(match (status, failure.code.as_str()) {
                (StatusCode::NOT_FOUND, "not_found") => NativeRegistryError::NotFound,
                (StatusCode::UNPROCESSABLE_ENTITY, "validation_failed") => {
                    NativeRegistryError::InvalidRequest
                }
                (StatusCode::CONFLICT, "native_object_conflict" | "native_object_ambiguous") => {
                    NativeRegistryError::Conflict
                }
                _ => NativeRegistryError::Unavailable,
            });
        }
        // 解码错误会携带原始字段值，既不记录错误文本，也不转发内部响应。
        serde_json::from_slice(&bytes).map_err(|_| NativeRegistryError::Unavailable)
    }
}

#[derive(Deserialize)]
struct AuthorityFailure {
    code: String,
}

impl NativeObjectRegistry for NativeObjectsClient {
    fn begin(&self, request: BeginRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("begin", &request).await })
    }

    fn bind(&self, request: BindRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("bind", &request).await })
    }

    fn lookup(&self, request: LookupRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("lookup", &request).await })
    }

    fn owned(&self, request: OwnedRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("owned", &request).await })
    }

    fn tombstone(&self, request: TombstoneRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("tombstone", &request).await })
    }

    fn claim(&self, request: ClaimRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("claim", &request).await })
    }

    fn renew(&self, request: LeaseRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("renew", &request).await })
    }

    fn reschedule(&self, request: LeaseRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("reschedule", &request).await })
    }

    fn complete(&self, request: CompleteRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("complete", &request).await })
    }
    fn signed_sip_intake(&self, request: SignedSipIntakeRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("live/sip-intake", &request).await })
    }
    fn mint_credential(&self, request: MintCredentialRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("credentials/mint", &request).await })
    }
    fn redeem_credential(&self, request: RedeemCredentialRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("credentials/redeem", &request).await })
    }
    fn create_live_session(&self, request: CreateLiveSessionRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("live/create", &request).await })
    }
    fn append_live_usage(&self, request: AppendLiveUsageRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("live/append", &request).await })
    }
    fn checkpoint_live_session(&self, request: CheckpointLiveSessionRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("live/checkpoint", &request).await })
    }
    fn claim_live_sessions(&self, request: ClaimLiveSessionsRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("live/claim", &request).await })
    }
    fn reserve_delete(&self, request: ObjectDeletionRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("delete/reserve", &request).await })
    }
    fn commit_delete(&self, request: ObjectDeletionRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("delete/commit", &request).await })
    }
    fn complete_typed(&self, request: TypedCompleteRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("complete-typed", &request).await })
    }
    fn mark_reconciliation(&self, request: ReconciliationRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("reconciliation", &request).await })
    }
    fn begin_related(&self, request: BeginRelatedRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("related/begin", &request).await })
    }
    fn bind_related(&self, request: BindRelatedRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("related/bind", &request).await })
    }
    fn reserve_relations(&self, request: ReserveRelationsRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("relations/reserve", &request).await })
    }

    fn commit_relations(&self, request: CommitRelationsRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("relations/commit", &request).await })
    }

    fn lookup_relations(&self, request: LookupRelationsRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("relations/lookup", &request).await })
    }

    fn resolve_reservation(&self, request: ResolveReservationRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("relations/resolve", &request).await })
    }

    fn bind_observed_artifacts(&self, request: BindObservedArtifactsRequest) -> ReplyFuture<'_> {
        Box::pin(async move { self.post("observed-artifacts", &request).await })
    }
}

#[cfg(test)]
mod tests {
    use aisix_core::native_objects::{
        ClaimRequest, LookupRequest, NativeObjectKind, NativeProtocol, ObjectLocator,
    };
    use aisix_proxy::native_api::registry::{NativeObjectRegistry, NativeRegistryError};
    use serde_json::json;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn http_client() -> reqwest::Client {
        let _ = rustls::crypto::ring::default_provider().install_default();
        aisix_gateway::control_plane_client_builder()
            .build()
            .unwrap()
    }

    fn client(server: &MockServer) -> NativeObjectsClient {
        NativeObjectsClient::with_client(&format!("{}/", server.uri()), http_client())
    }

    fn lookup_request() -> LookupRequest {
        LookupRequest {
            consumer_family: None,
            api_key_id: uuid::Uuid::from_u128(1),
            locator: ObjectLocator {
                protocol: NativeProtocol::Openai,
                kind: NativeObjectKind::Response,
                raw_id: "resp_opaque/native?value".to_owned(),
                origin: None,
                parent_id: None,
            },
        }
    }

    // 改成 GET、把原生 ID 拼到路径或漏掉调用方密钥，均不能得到 found。
    #[tokio::test]
    async fn lookup_posts_authority_inputs_and_decodes_newer_replies() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/dp/v1/native-objects/lookup"))
            .and(header("content-type", "application/json"))
            .and(body_json(json!({
                "api_key_id": "00000000-0000-0000-0000-000000000001",
                "locator": {
                    "protocol": "openai", "kind": "response",
                    "raw_id": "resp_opaque/native?value", "origin": null, "parent_id": null
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "state": "found", "future_field": {"private": true},
                "record": {"record_id": "00000000-0000-0000-0000-000000000002", "future": 1}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let reply = client(&server).lookup(lookup_request()).await.unwrap();
        assert_eq!(reply.state, "found");
        assert_eq!(reply.record.unwrap().record_id, uuid::Uuid::from_u128(2));
    }

    // 503 不能伪装成对象不存在；账号边界变化也不能变成可重选账号的冲突。
    #[tokio::test]
    async fn errors_distinguish_missing_conflicting_and_unavailable_authority() {
        for (status, body, expected) in [
            (
                404,
                json!({"code": "not_found"}),
                NativeRegistryError::NotFound,
            ),
            (
                409,
                json!({"code": "native_object_conflict"}),
                NativeRegistryError::Conflict,
            ),
            (
                409,
                json!({"code": "native_object_ambiguous"}),
                NativeRegistryError::Conflict,
            ),
            (
                409,
                json!({"code": "native_origin_changed"}),
                NativeRegistryError::Unavailable,
            ),
            (
                422,
                json!({"code": "validation_failed"}),
                NativeRegistryError::InvalidRequest,
            ),
            (
                503,
                json!({"code": "unavailable"}),
                NativeRegistryError::Unavailable,
            ),
            (
                401,
                json!({"code": "unauthenticated"}),
                NativeRegistryError::Unavailable,
            ),
            (
                409,
                json!({"code": "new_conflict_kind"}),
                NativeRegistryError::Unavailable,
            ),
            (
                404,
                json!({"code": "new_missing_kind"}),
                NativeRegistryError::Unavailable,
            ),
            (
                422,
                json!({"code": "new_validation_kind"}),
                NativeRegistryError::Unavailable,
            ),
            (
                404,
                json!({"code": "validation_failed"}),
                NativeRegistryError::Unavailable,
            ),
            (
                422,
                json!({"code": "not_found"}),
                NativeRegistryError::Unavailable,
            ),
            (404, json!({"code": null}), NativeRegistryError::Unavailable),
            (
                422,
                json!({"future_code": "validation_failed"}),
                NativeRegistryError::Unavailable,
            ),
        ] {
            let server = MockServer::builder().start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_json(body))
                .expect(1)
                .mount(&server)
                .await;
            assert!(
                matches!(client(&server).lookup(lookup_request()).await, Err(e) if e == expected),
                "status {status}"
            );
        }
    }

    // 删除 state 时必须拒绝；其他未来字段可忽略，未知状态留给调用方闭合判断。
    #[tokio::test]
    async fn malformed_success_is_unavailable() {
        for body in ["{}", "null", "{\"state\": 3}", "not-json"] {
            let server = MockServer::builder().start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .expect(1)
                .mount(&server)
                .await;
            assert!(matches!(
                client(&server).lookup(lookup_request()).await,
                Err(NativeRegistryError::Unavailable)
            ));
        }
    }

    // 老控制面尚无此 API 时的空 404 不能伪装成对象不存在。
    #[tokio::test]
    async fn missing_authority_endpoint_is_unavailable() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        assert!(matches!(
            client(&server).lookup(lookup_request()).await,
            Err(NativeRegistryError::Unavailable)
        ));
    }

    // 没有 Content-Length 的分块响应也必须受同一上限约束。
    #[tokio::test]
    async fn oversized_chunked_success_is_unavailable() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            let payload = serde_json::to_vec(&json!({
                "state": "found", "future": "x".repeat(MAX_REPLY_BYTES + 1)
            }))
            .unwrap();
            let mut wire = format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
                payload.len()
            )
            .into_bytes();
            wire.extend_from_slice(&payload);
            wire.extend_from_slice(b"\r\n0\r\n\r\n");
            let _ = socket.write_all(&wire).await;
        });
        let client = NativeObjectsClient::with_client(&format!("http://{addr}"), http_client());
        let result = client.lookup(lookup_request()).await;
        peer.await.unwrap();
        assert!(matches!(result, Err(NativeRegistryError::Unavailable)));
    }

    // 回复无限增长时不能不受限制地分配内存。
    #[tokio::test]
    async fn oversized_success_is_unavailable() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "state": "found", "future": "x".repeat(MAX_REPLY_BYTES + 1)
            })))
            .expect(1)
            .mount(&server)
            .await;
        assert!(matches!(
            client(&server).lookup(lookup_request()).await,
            Err(NativeRegistryError::Unavailable)
        ));
    }

    // 错误回复同样有界，不能只凭状态码提前转换为对象或调用方错误。
    #[tokio::test]
    async fn oversized_known_error_is_unavailable() {
        for (status, code) in [
            (404, "not_found"),
            (422, "validation_failed"),
            (409, "native_object_conflict"),
        ] {
            let server = MockServer::builder().start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                    "code": code, "future": "x".repeat(MAX_REPLY_BYTES + 1)
                })))
                .expect(1)
                .mount(&server)
                .await;
            assert!(
                matches!(
                    client(&server).lookup(lookup_request()).await,
                    Err(NativeRegistryError::Unavailable)
                ),
                "status {status}"
            );
        }
    }

    // 重定向不能把调用方身份或内部归属载荷带到另一个服务。
    #[tokio::test]
    async fn redirect_is_not_followed() {
        let server = MockServer::builder().start().await;
        let other = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(307).insert_header("location", other.uri()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"state": "found"})))
            .expect(0)
            .mount(&other)
            .await;
        assert!(matches!(
            client(&server).lookup(lookup_request()).await,
            Err(NativeRegistryError::Unavailable)
        ));
    }

    // 解码错误中可能含原始字段值，不能直接记录 serde 的错误文本。
    #[tokio::test]
    async fn rejected_reply_does_not_log_private_body_values() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "state": "found", "record": {
                    "record_id": "00000000-0000-0000-0000-000000000002",
                    "protocol": "private-provider-credential"
                }
            })))
            .mount(&server)
            .await;
        let client = client(&server);
        let (reply, logged) =
            crate::dp_store::test_logs::capture_logs(client.lookup(lookup_request())).await;
        assert!(matches!(reply, Err(NativeRegistryError::Unavailable)));
        assert!(!logged.contains("private-provider-credential"), "{logged}");
    }

    // 只限连接、不限响应体会让已经发出响应头的控制面永久占住调用方。
    #[tokio::test]
    async fn deadline_includes_a_stalled_response_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
                .await
                .unwrap();
            let _ = held.await;
        });
        let client = NativeObjectsClient::with_client(&format!("http://{addr}"), http_client());
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.lookup(lookup_request()),
        )
        .await;
        let _ = release.send(());
        peer.await.unwrap();
        assert!(matches!(result, Ok(Err(NativeRegistryError::Unavailable))));
    }

    // 领取页保留记录数；空 records 不能当作请求失败而重试。
    #[tokio::test]
    async fn empty_claim_is_successful_and_not_retried() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/dp/v1/native-objects/claim"))
            .and(body_json(json!({"limit": 10, "record_id": null})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"state": "leased", "records": []})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let reply = client(&server)
            .claim(ClaimRequest {
                limit: 10,
                record_id: None,
                protocol: None,
                kind: None,
            })
            .await
            .unwrap();
        assert_eq!(reply.state, "leased");
        assert!(reply.records.is_empty());
    }
}
