//! `/v1/messages` and `/v1/messages/count_tokens` for Claude on Bedrock,
//! served on Bedrock's own Anthropic Messages routes: `InvokeModel`,
//! `InvokeModelWithResponseStream` and `CountTokens`. The caller's body
//! reaches Claude as it was sent, apart from what
//! [`aisix_provider_anthropic::platform::shape_request`] adapts, and the
//! response comes back in the same wire.

use std::sync::OnceLock;
use std::time::Instant;

use crate::upstream_io::UpstreamIoInterceptor;
use aisix_gateway::media_fetch::{fetch_urls, Encoding};
use aisix_gateway::upstream_io::Protocol;
use aisix_gateway::{BridgeContext, BridgeError, NativeMessages};
use aisix_provider_anthropic::platform::{self, Platform};
use aws_sdk_bedrockruntime::error::SdkError;
use aws_sdk_bedrockruntime::primitives::Blob;
use aws_sdk_bedrockruntime::types::error::ResponseStreamError;
use aws_sdk_bedrockruntime::types::{CountTokensInput, InvokeModelTokensRequest, ResponseStream};
use serde_json::Value;

use crate::bridge::{
    strip_region_prefix, upstream_model, validate_model_id_chars, within_attempt, BedrockBridge,
    BedrockPublisher, BedrockSecret,
};

/// Whether `/v1/messages` for `upstream_id` takes Bedrock's own Anthropic
/// route: an `anthropic.` model id, region prefix or not, and an ARN that
/// names a Claude model. An ARN that hides its model (an application
/// inference profile) keeps the chat translation.
pub(crate) fn serves_anthropic_messages(upstream_id: &str) -> bool {
    if upstream_id.starts_with("arn:") {
        return upstream_id.contains("claude");
    }
    BedrockPublisher::from_model_id(upstream_id) == Some(BedrockPublisher::Anthropic)
}

impl BedrockBridge {
    pub(crate) async fn native_messages(
        &self,
        body: Value,
        ctx: &BridgeContext,
    ) -> Result<NativeMessages, BridgeError> {
        within_attempt(
            ctx.deadline,
            Instant::now(),
            self.native_messages_attempt(body, ctx),
        )
        .await
    }

    async fn native_messages_attempt(
        &self,
        mut body: Value,
        ctx: &BridgeContext,
    ) -> Result<NativeMessages, BridgeError> {
        let upstream_id = upstream_model(ctx)?;
        validate_model_id_chars(upstream_id)?;
        let stream = body.get("stream").and_then(Value::as_bool) == Some(true);
        let fetched = fetch_urls(platform::media_urls(&body), ctx, Encoding::Base64).await?;
        let charged = fetched.charge(ctx);
        let ctx = &*charged;
        platform::inline_media(&mut body, Platform::Bedrock, &fetched)
            .map_err(BridgeError::InvalidRequest)?;
        drop(fetched);
        let mut betas = platform::header_betas(ctx.client_headers.as_deref());
        betas.extend(crate::bridge::header_betas(ctx));
        platform::shape_request(&mut body, Platform::Bedrock, upstream_id, betas);
        let bytes = Vec::from(
            aisix_gateway::json_body(&body)
                .map_err(|e| BridgeError::Config(format!("serialize request body: {e}")))?,
        );
        drop(body);

        let client = self.build_client_from_ctx(ctx).await?;
        let started = Instant::now();
        let deadline = ctx.deadline;
        if !stream {
            let resp = client
                .invoke_model()
                .model_id(upstream_id)
                .content_type("application/json")
                .accept("application/json")
                .body(Blob::new(bytes))
                .customize()
                .interceptor(UpstreamIoInterceptor::new(Protocol::Messages))
                .send()
                .await
                .map_err(|e| crate::errors::sdk_error(e, started, deadline))?;
            let json: Value = serde_json::from_slice(resp.body().as_ref())
                .map_err(|e| BridgeError::UpstreamDecode(e.to_string()))?;
            return Ok(NativeMessages::Json(json));
        }

        let io = UpstreamIoInterceptor::new(Protocol::Messages);
        let mut resp = client
            .invoke_model_with_response_stream()
            .model_id(upstream_id)
            .content_type("application/json")
            .accept("application/json")
            .body(Blob::new(bytes))
            .customize()
            .interceptor(io.clone())
            .send()
            .await
            .map_err(|e| crate::errors::sdk_error(e, started, deadline))?;

        // The first event is awaited here, before the response is committed,
        // so an exception Bedrock raises before any content (a throttle is
        // the common one) fails this attempt and the caller can fail over.
        let first = match deadline {
            Some(d) => tokio::time::timeout(d.saturating_sub(started.elapsed()), resp.body.recv())
                .await
                .map_err(|_| BridgeError::Timeout {
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    cause: String::new(),
                })?,
            None => resp.body.recv().await,
        };
        let first = match first {
            Ok(Some(event)) => event_bytes(event, &io),
            Ok(None) => return Err(BridgeError::StreamAborted),
            Err(e) => return Err(stream_error(&e).into_bridge_error()),
        };
        let frames = async_stream::stream! {
            if let Some(frame) = first {
                yield Ok(frame);
            }
            loop {
                match resp.body.recv().await {
                    Ok(Some(event)) => {
                        if let Some(frame) = event_bytes(event, &io) {
                            yield Ok(frame);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let err = stream_error(&e);
                        yield Ok(platform::error_frame(err.kind, &err.message));
                        break;
                    }
                }
            }
        };
        Ok(NativeMessages::Stream(Box::pin(frames)))
    }

    pub(crate) async fn native_count_tokens(
        &self,
        body: Value,
        ctx: &BridgeContext,
    ) -> Result<Value, BridgeError> {
        within_attempt(
            ctx.deadline,
            Instant::now(),
            self.native_count_tokens_attempt(body, ctx),
        )
        .await
    }

    async fn native_count_tokens_attempt(
        &self,
        body: Value,
        ctx: &BridgeContext,
    ) -> Result<Value, BridgeError> {
        let upstream_id = upstream_model(ctx)?;
        validate_model_id_chars(upstream_id)?;
        // CountTokens names the base model: no inference-profile prefix, no
        // ARN path.
        let tail = upstream_id.rsplit('/').next().unwrap_or(upstream_id);
        let model_id = strip_region_prefix(tail);
        let mut betas = platform::header_betas(ctx.client_headers.as_deref());
        betas.extend(crate::bridge::header_betas(ctx));
        let invoke =
            platform::bedrock_count_tokens_invoke_body_with_betas(&body, upstream_id, betas);
        let io = UpstreamIoInterceptor::new(Protocol::Messages).with_model_input(&invoke);
        let bytes = Vec::from(
            aisix_gateway::json_body(&invoke)
                .map_err(|e| BridgeError::Config(format!("serialize request body: {e}")))?,
        );
        let input = CountTokensInput::InvokeModel(
            InvokeModelTokensRequest::builder()
                .body(Blob::new(bytes))
                .build()
                .map_err(|e| BridgeError::Config(e.to_string()))?,
        );
        let client = self.build_client_from_ctx(ctx).await?;
        let started = Instant::now();
        let runtime = client
            .count_tokens()
            .model_id(model_id)
            .input(input)
            .customize()
            .interceptor(io)
            .send()
            .await
            .map_err(|e| crate::errors::sdk_error(e, started, ctx.deadline));
        match runtime {
            Ok(resp) => Ok(serde_json::json!({"input_tokens": resp.input_tokens()})),
            // bedrock-runtime's CountTokens does not count every Claude model
            // (those Bedrock serves only through cross-region inference); AWS
            // counts them on Bedrock Mantle's Anthropic count_tokens instead.
            Err(BridgeError::UpstreamStatus {
                status: 400 | 404, ..
            }) if model_id.to_ascii_lowercase().contains("claude") => {
                mantle_count_tokens(&body, model_id, ctx).await
            }
            Err(e) => Err(e),
        }
    }
}

static MANTLE_ENDPOINT: OnceLock<Option<String>> = OnceLock::new();

/// Install `upstream.bedrock.mantle_endpoint`. Called once during boot.
pub fn init_mantle_endpoint(endpoint: Option<String>) {
    let _ = MANTLE_ENDPOINT.set(endpoint.filter(|e| !e.trim().is_empty()));
}

const MANTLE_MESSAGES_PATH: &str = "/anthropic/v1/messages";

/// `{mantle}/anthropic/v1/messages/count_tokens`, where `{mantle}` is the
/// configured endpoint (which may already end in the messages path) or the
/// public regional host.
fn mantle_count_tokens_url(region: &str) -> String {
    let base = match MANTLE_ENDPOINT.get().cloned().flatten() {
        Some(configured) => {
            let base = configured.trim().trim_end_matches('/').to_string();
            if base.ends_with(MANTLE_MESSAGES_PATH) {
                base
            } else {
                format!("{base}{MANTLE_MESSAGES_PATH}")
            }
        }
        None => format!("https://bedrock-mantle.{region}.api.aws{MANTLE_MESSAGES_PATH}"),
    };
    format!("{base}/count_tokens")
}

/// Count on Bedrock Mantle's Anthropic `count_tokens`: the base model id,
/// the request's messages, `system` and `tools`, and the options counting
/// reads, authenticated with an API key or SigV4 for the Mantle surface.
async fn mantle_count_tokens(
    body: &Value,
    model_id: &str,
    ctx: &BridgeContext,
) -> Result<Value, BridgeError> {
    let creds = BedrockSecret::parse(&ctx.provider_key.api_key)?;
    let mut count = serde_json::Map::new();
    count.insert("model".to_string(), Value::String(model_id.to_string()));
    for key in [
        "messages",
        "system",
        "tools",
        "thinking",
        "tool_choice",
        "output_config",
        "compaction",
        "context_management",
    ] {
        if let Some(v) = body.get(key).filter(|v| !v.is_null()) {
            count.insert(key.to_string(), v.clone());
        }
    }
    // A display cannot change the count, and Bedrock refuses some values.
    if let Some(thinking) = count.get_mut("thinking").and_then(Value::as_object_mut) {
        thinking.remove("display");
    }
    let bytes = Vec::from(
        aisix_gateway::json_body(&aisix_provider_anthropic::wire::DefaultCacheTtl::request(
            &Value::Object(count),
        ))
        .map_err(|e| BridgeError::Config(format!("serialize request body: {e}")))?,
    );
    let url = mantle_count_tokens_url(creds.region());

    let mut unsigned = vec![
        ("content-type".to_string(), "application/json".to_string()),
        (
            "anthropic-version".to_string(),
            ctx.client_headers
                .as_ref()
                .and_then(|headers| headers.get("anthropic-version"))
                .and_then(|value| value.to_str().ok())
                .unwrap_or("2023-06-01")
                .to_string(),
        ),
    ];
    let mut betas = platform::header_betas(ctx.client_headers.as_deref());
    betas.extend(crate::bridge::header_betas(ctx));
    let shaped = platform::bedrock_count_tokens_invoke_body_with_betas(body, model_id, betas);
    if let Some(betas) = shaped.get("anthropic_beta").and_then(Value::as_array) {
        let betas = betas
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(",");
        if !betas.is_empty() {
            unsigned.push(("anthropic-beta".to_string(), betas));
        }
    }
    let auth = creds.request_auth(ctx.deadline).await?;
    let signed =
        crate::bridge::mantle_auth_headers(&auth, creds.region(), &url, &unsigned, &bytes)?;

    let client = aisix_gateway::upstream_tls::raw_client_for_provider_key(
        ctx.provider_key.upstream_connection().as_ref(),
    );
    let endpoint = aisix_gateway::url_cache::cached_endpoint_url(
        &ctx.provider_key_id,
        "bedrock/mantle-count-tokens",
        &[&url],
        || Ok::<_, BridgeError>(url.clone()),
    )?;
    let mut req = endpoint.post_on(&client).body(bytes);
    for (name, value) in unsigned.iter().chain(signed.iter()) {
        req = req.header(name, value);
    }
    if let Some(d) = ctx.deadline {
        req = req.timeout(d);
    }
    let started = Instant::now();
    let resp = aisix_gateway::upstream_io::send(req, Protocol::Messages)
        .await
        .map_err(aisix_gateway::send_error)?;
    aisix_gateway::note_upstream_response(resp.headers());
    let status = resp.status();
    if !status.is_success() {
        let retry_after = aisix_gateway::parse_retry_after(resp.headers());
        let kind = resp
            .headers()
            .get("x-amzn-errortype")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body =
            aisix_gateway::read_body_capped(resp, aisix_gateway::MAX_UPSTREAM_ERROR_BODY_BYTES)
                .await;
        return Err(crate::errors::upstream_error(
            status.as_u16(),
            kind,
            None,
            aisix_gateway::upstream_error_message(&body.bytes),
            retry_after,
            started,
        )
        .with_body_read_error(body.read_error));
    }
    let json: Value = resp
        .json()
        .await
        .map_err(|e| aisix_gateway::upstream_body_read_error(&e))?;
    let tokens = json
        .get("input_tokens")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            BridgeError::UpstreamDecode("count_tokens answer carries no input_tokens".into())
        })?;
    Ok(serde_json::json!({"input_tokens": tokens}))
}

/// One stream event as an Anthropic SSE frame. A `chunk` carries the
/// Anthropic event JSON itself.
fn event_bytes(event: ResponseStream, io: &UpstreamIoInterceptor) -> Option<bytes::Bytes> {
    match event {
        ResponseStream::Chunk(part) => part.bytes.map(|b| {
            if let Some(io) = io.observer() {
                crate::upstream_io::observe_native_event(io, b.as_ref());
            }
            platform::event_frame(b.as_ref())
        }),
        _ => None,
    }
}

/// An exception Bedrock raised inside the event stream, in Anthropic's
/// error taxonomy so an Anthropic client's retry logic reads it.
struct StreamException {
    kind: &'static str,
    status: Option<u16>,
    message: String,
    code: Option<String>,
}

fn stream_error(
    e: &SdkError<ResponseStreamError, aws_smithy_types::event_stream::RawMessage>,
) -> StreamException {
    let Some(svc) = e.as_service_error() else {
        return StreamException {
            kind: "api_error",
            status: None,
            message: "upstream stream failed".to_string(),
            code: None,
        };
    };
    let (kind, status) = match svc {
        ResponseStreamError::ThrottlingException(_) => ("rate_limit_error", Some(429)),
        ResponseStreamError::ServiceUnavailableException(_) => ("overloaded_error", Some(529)),
        ResponseStreamError::ValidationException(_) => ("invalid_request_error", Some(400)),
        ResponseStreamError::ModelTimeoutException(_) => ("timeout_error", Some(504)),
        ResponseStreamError::InternalServerException(_) => ("api_error", Some(500)),
        // The model backend's own status, when Bedrock relays one.
        ResponseStreamError::ModelStreamErrorException(inner) => {
            match inner.original_status_code() {
                Some(429) => ("rate_limit_error", Some(429)),
                Some(503 | 529) => ("overloaded_error", Some(529)),
                Some(400) => ("invalid_request_error", Some(400)),
                Some(c) => ("api_error", u16::try_from(c).ok()),
                None => ("api_error", Some(502)),
            }
        }
        _ => ("api_error", None),
    };
    // The upstream's own text; a status-keyed phrase only when it sent none.
    let message = svc
        .meta()
        .message()
        .filter(|m| !m.is_empty())
        .map(|m| {
            aisix_gateway::truncate_lossy(
                &aisix_gateway::scrub_upstream_error_text(m),
                aisix_gateway::MAX_UPSTREAM_ERROR_MESSAGE_BYTES,
            )
        })
        .unwrap_or_else(|| match status {
            Some(s) => format!("upstream reported stream error {s}"),
            None => "upstream reported a stream error".to_string(),
        });
    StreamException {
        kind,
        status,
        message,
        code: svc.meta().code().map(str::to_string),
    }
}

impl StreamException {
    fn into_bridge_error(self) -> BridgeError {
        BridgeError::UpstreamInBand {
            status: self.status,
            message: self.message,
            parsed: Some(Box::new(aisix_gateway::UpstreamErrorView {
                kind: self.code.or(Some(self.kind.to_string())),
                message: None,
                code: None,
                param: None,
            })),
            wire: aisix_gateway::UpstreamWire::Bedrock,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_ids_take_the_anthropic_route() {
        for id in [
            "anthropic.claude-sonnet-4-5-20250929-v1:0",
            "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.claude-sonnet-4-5-v1:0",
        ] {
            assert!(serves_anthropic_messages(id), "{id}");
        }
        for id in [
            "meta.llama3-3-70b-instruct-v1:0",
            "openai.gpt-oss-120b-1:0",
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abcd1234",
        ] {
            assert!(!serves_anthropic_messages(id), "{id}");
        }
    }
}

#[cfg(test)]
mod compat_claude_native_tests {
    use super::*;
    use std::sync::Arc;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn context(secret: &str) -> BridgeContext {
        let model = serde_json::from_value(serde_json::json!({
            "display_name":"native-claude","provider":"bedrock","provider_key_id":"pk",
            "model_name":"us.anthropic.claude-opus-5-5"
        }))
        .unwrap();
        let pk = serde_json::from_value(
            serde_json::json!({"display_name":"native-pk","api_key":secret}),
        )
        .unwrap();
        BridgeContext::new("req-native-auth", Arc::new(model), Arc::new(pk))
    }

    #[derive(Debug)]
    struct LocalCredentialConnector {
        inner: aws_smithy_runtime_api::client::http::SharedHttpConnector,
        endpoint: String,
    }

    #[cfg(test)]
    fn credential_http_client() -> aws_smithy_runtime_api::client::http::SharedHttpClient {
        aisix_gateway::upstream_tls::build_aws_http_client()
    }

    impl aws_smithy_runtime_api::client::http::HttpConnector for LocalCredentialConnector {
        fn call(
            &self,
            mut request: aws_smithy_runtime_api::client::orchestrator::HttpRequest,
        ) -> aws_smithy_runtime_api::client::http::HttpConnectorFuture {
            request.set_uri(self.endpoint.clone()).unwrap();
            self.inner.call(request)
        }
    }

    async fn native_slow_auth_and_response_share_attempt_deadline(
        operation: &str,
        first_event: bool,
    ) {
        use aws_smithy_runtime_api::client::http::{
            http_client_fn, HttpClient, SharedHttpConnector,
        };
        use std::time::Duration;

        // 只替换 STS 的网络地址；SDK 的 AssumeRole、签名与响应解码仍走真实 TCP。
        let sts = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("Action=GetCallerIdentity"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<GetCallerIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
                <GetCallerIdentityResult><Arn>arn:aws:iam::123456789012:user/component</Arn>
                <UserId>component</UserId><Account>123456789012</Account></GetCallerIdentityResult>
                <ResponseMetadata><RequestId>component-identity</RequestId></ResponseMetadata>
                </GetCallerIdentityResponse>"#,
            ))
            .expect(1)
            .mount(&sts)
            .await;
        let xml = r#"<AssumeRoleResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
            <AssumeRoleResult><Credentials>
            <AccessKeyId>ASIA-component-test</AccessKeyId>
            <SecretAccessKey>component-secret</SecretAccessKey>
            <SessionToken>component-session</SessionToken>
            <Expiration>2099-01-01T00:00:00Z</Expiration>
            </Credentials><AssumedRoleUser>
            <AssumedRoleId>component:deadline</AssumedRoleId>
            <Arn>arn:aws:sts::123456789012:assumed-role/component/deadline</Arn>
            </AssumedRoleUser></AssumeRoleResult>
            <ResponseMetadata><RequestId>component-request</RequestId></ResponseMetadata>
            </AssumeRoleResponse>"#;
        Mock::given(method("POST"))
            .and(body_string_contains("Action=AssumeRole"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/xml")
                    .set_body_string(xml)
                    .set_delay(Duration::from_millis(400)),
            )
            .expect(1)
            .mount(&sts)
            .await;
        let endpoint = sts.uri();
        let credentials_http = credential_http_client();
        let credentials_http = http_client_fn(move |settings, components| {
            SharedHttpConnector::new(LocalCredentialConnector {
                inner: credentials_http.http_connector(settings, components),
                endpoint: endpoint.clone(),
            })
        });
        let runtime = MockServer::builder().start().await;
        let mut event_body = None;
        let response = if operation == "invoke-with-response-stream" {
            use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
            use base64::Engine as _;
            let chunk = serde_json::to_vec(&serde_json::json!({"bytes":
                base64::engine::general_purpose::STANDARD.encode(br#"{"type":"ping"}"#)
            }))
            .unwrap();
            let event = Message::new(bytes::Bytes::from(chunk))
                .add_header(Header::new(
                    ":message-type",
                    HeaderValue::String("event".into()),
                ))
                .add_header(Header::new(
                    ":event-type",
                    HeaderValue::String("chunk".into()),
                ))
                .add_header(Header::new(
                    ":content-type",
                    HeaderValue::String("application/json".into()),
                ));
            let mut wire = Vec::new();
            aws_smithy_eventstream::frame::write_message_to(&event, &mut wire).unwrap();
            event_body = Some(wire.clone());
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(wire)
        } else if operation == "count-tokens" {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"inputTokens": 1}))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "type":"message","content":[{"type":"text","text":"ok"}]
            }))
        };
        let model = if operation == "count-tokens" {
            "anthropic.claude-opus-5-5"
        } else {
            "us.anthropic.claude-opus-5-5"
        };
        let (endpoint, reply) = if first_event {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let wire = event_body.unwrap();
            let reply = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                assert!(socket.read(&mut request).await.unwrap() > 0);
                let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/vnd.amazon.eventstream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", wire.len());
                socket.write_all(head.as_bytes()).await.unwrap();
                tokio::time::sleep(Duration::from_millis(400)).await;
                let _ = socket.write_all(&wire).await;
            });
            (endpoint, Some(reply))
        } else {
            Mock::given(method("POST"))
                .and(path(format!("/model/{model}/{operation}")))
                .respond_with(response.set_delay(Duration::from_millis(400)))
                .expect(1)
                .mount(&runtime)
                .await;
            (runtime.uri(), None)
        };
        let bridge = BedrockBridge::new()
            .with_endpoint_override(endpoint)
            .with_credentials_http(credentials_http);
        let secret = r#"{"access_key_id":"AKIA-component-test","secret_access_key":"component-secret",
            "region":"us-east-1","role_arn":"arn:aws:iam::123456789012:role/component"}"#;
        let ctx = context(secret).with_deadline(Duration::from_millis(600));
        let body = serde_json::json!({
            "messages":[{"role":"user","content":"hi"}],
            "max_tokens":1,"stream":operation == "invoke-with-response-stream"
        });
        let started = Instant::now();
        let result = if operation == "count-tokens" {
            bridge.native_count_tokens(body, &ctx).await.map(|_| ())
        } else {
            bridge.native_messages(body, &ctx).await.map(|_| ())
        };
        assert!(
            matches!(result, Err(BridgeError::Timeout { .. })),
            "{operation}: slow auth plus response must exhaust one budget: {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "{operation}: restarted the deadline after authentication: {:?}",
            started.elapsed()
        );
        if let Some(reply) = reply {
            reply.await.unwrap();
        }
    }

    #[tokio::test]
    async fn native_messages_share_auth_and_invoke_deadline() {
        native_slow_auth_and_response_share_attempt_deadline("invoke", false).await;
    }

    #[tokio::test]
    async fn native_messages_stream_share_auth_and_invoke_deadline() {
        native_slow_auth_and_response_share_attempt_deadline("invoke-with-response-stream", false)
            .await;
    }

    #[tokio::test]
    async fn native_messages_stream_first_event_share_auth_and_invoke_deadline() {
        native_slow_auth_and_response_share_attempt_deadline("invoke-with-response-stream", true)
            .await;
    }

    #[tokio::test]
    async fn native_count_tokens_share_auth_and_invoke_deadline() {
        native_slow_auth_and_response_share_attempt_deadline("count-tokens", false).await;
    }

    #[tokio::test]
    async fn compat_claude_native_mantle_authentication_after_runtime_fallback() {
        let server = MockServer::builder().start().await;
        init_mantle_endpoint(Some(server.uri()));
        Mock::given(method("POST")).and(path("/model/anthropic.claude-opus-5-5/count-tokens"))
            .respond_with(ResponseTemplate::new(400).insert_header("x-amzn-errortype","ValidationException").set_body_json(serde_json::json!({"message":"The provided model does not support counting tokens."})))
            .mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/anthropic/v1/messages/count_tokens"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"input_tokens":77})),
            )
            .mount(&server)
            .await;
        let bridge = BedrockBridge::new().with_endpoint_override(server.uri());
        for secret in [
            r#"{"auth_type":"api_key","api_key":"ABSK-native-test","region":"us-east-1"}"#,
            r#"{"access_key_id":"AKIA-native-test","secret_access_key":"synthetic-secret","region":"us-east-1"}"#,
        ] {
            let result = bridge
                .native_count_tokens(
                    serde_json::json!({"messages":[{"role":"user","content":"hi"}]}),
                    &context(secret),
                )
                .await
                .unwrap();
            assert_eq!(result["input_tokens"], 77);
        }
        let requests = server.received_requests().await.unwrap();
        let mantle = requests
            .iter()
            .filter(|r| r.url.path() == "/anthropic/v1/messages/count_tokens")
            .collect::<Vec<_>>();
        assert_eq!(mantle.len(), 2);
        assert_eq!(
            mantle[0]
                .headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok()),
            Some("ABSK-native-test")
        );
        assert!(!mantle[0].headers.contains_key("authorization"));
        let scope = mantle[1]
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            scope.contains("/us-east-1/bedrock-mantle/aws4_request"),
            "{scope}"
        );
        let runtime = requests
            .iter()
            .filter(|r| r.url.path().ends_with("/count-tokens"))
            .collect::<Vec<_>>();
        assert_eq!(runtime.len(), 2);
        assert_eq!(
            runtime[0].headers.get("authorization").unwrap(),
            "Bearer ABSK-native-test"
        );
        assert!(runtime[1]
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("/us-east-1/bedrock/aws4_request"));

        // Runtime 拒绝后，Mantle 回退仍须花同一份计数预算。
        let _runtime = Mock::given(method("POST"))
            .and(path("/model/anthropic.claude-opus-5-5/count-tokens"))
            .respond_with(
                ResponseTemplate::new(400)
                    .insert_header("x-amzn-errortype", "ValidationException")
                    .set_body_json(serde_json::json!({"message":"The provided model does not support counting tokens."}))
                    .set_delay(std::time::Duration::from_millis(400)),
            )
            .with_priority(1)
            .expect(1)
            .mount_as_scoped(&server)
            .await;
        let _mantle = Mock::given(method("POST"))
            .and(path("/anthropic/v1/messages/count_tokens"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"input_tokens":77}))
                    .set_delay(std::time::Duration::from_millis(400)),
            )
            .with_priority(1)
            .expect(1)
            .mount_as_scoped(&server)
            .await;
        let result = bridge
            .native_count_tokens(
                serde_json::json!({"messages":[{"role":"user","content":"hi"}]}),
                &context(
                    r#"{"auth_type":"api_key","api_key":"ABSK-native-test","region":"us-east-1"}"#,
                )
                .with_deadline(std::time::Duration::from_millis(600)),
            )
            .await;
        assert!(
            matches!(result, Err(BridgeError::Timeout { .. })),
            "{result:?}"
        );
    }
}
