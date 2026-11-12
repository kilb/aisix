//! Bedrock 的已签名 HTTP 请求走同一个受出站策略保护的探测客户端。
//!
//! 此处只适配传输，不重试、不重定向，也不改变 SDK 的请求或事件流。
//! 契约来自锁定版本 aws-smithy-runtime-api 1.12.3 `client/http.rs`，
//! 及 aws-smithy-types 1.4.9 `body/http_body_1_x.rs`。

use std::sync::Arc;
use std::time::Duration;

use aws_smithy_runtime_api::client::http::{
    http_client_fn, HttpConnector, HttpConnectorFuture, SharedHttpClient, SharedHttpConnector,
};
use aws_smithy_runtime_api::client::orchestrator::{HttpRequest, HttpResponse};
use aws_smithy_runtime_api::client::result::ConnectorError;
use aws_smithy_types::body::SdkBody;
use futures::TryStreamExt;
use http_body_util::{BodyDataStream, StreamBody};

use crate::error::ApiError;
use crate::upstream_http;

pub(super) fn client(client: reqwest::Client) -> SharedHttpClient {
    http_client_fn(move |settings, _| {
        SharedHttpConnector::new(Connector {
            transport: Transport::Prepared(client.clone()),
            read_timeout: settings.read_timeout(),
        })
    })
}

/// STS 的端点可由 SDK 的环境或配置选择，必须在每次发送前检查实际地址。
pub(super) fn credential_client(
    outbound: Arc<upstream_http::Client>,
    timeout: Duration,
) -> SharedHttpClient {
    http_client_fn(move |settings, _| {
        SharedHttpConnector::new(Connector {
            transport: Transport::Credentials {
                outbound: Arc::clone(&outbound),
                timeout,
            },
            read_timeout: settings.read_timeout(),
        })
    })
}

#[derive(Clone)]
enum Transport {
    Prepared(reqwest::Client),
    Credentials {
        outbound: Arc<upstream_http::Client>,
        timeout: Duration,
    },
}

impl Transport {
    async fn client_for(&self, url: &reqwest::Url) -> Result<reqwest::Client, ConnectorError> {
        match self {
            Self::Prepared(client) => Ok(client.clone()),
            Self::Credentials { outbound, timeout } => {
                aisix_gateway::response_tap::note_url_userinfo(url.as_str());
                outbound
                    .credential_client(std::slice::from_ref(url), *timeout)
                    .await
                    .map_err(policy_error)
            }
        }
    }
}

struct Connector {
    transport: Transport,
    read_timeout: Option<Duration>,
}

impl std::fmt::Debug for Connector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeHttpConnector").finish_non_exhaustive()
    }
}

impl HttpConnector for Connector {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let transport = self.transport.clone();
        let read_timeout = self.read_timeout;
        HttpConnectorFuture::new(async move {
            let request = request_into_reqwest(request)?;
            let sending = async {
                let client = transport.client_for(request.url()).await?;
                client.execute(request).await.map_err(transport_error)
            };
            // 连接时限由受保护客户端固定为两秒，探测外层另有三十秒总界限。
            // Smithy 的 read_timeout 只界定从发起请求到响应头，不能截断完整事件流。
            let response = match read_timeout {
                Some(timeout) => tokio::time::timeout(timeout, sending).await.map_err(|_| {
                    ConnectorError::timeout(Box::new(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "模型探测等待上游响应超时",
                    )))
                })??,
                None => sending.await?,
            };
            response_into_smithy(response)
        })
    }
}

fn policy_error(error: ApiError) -> ConnectorError {
    let detail = std::iter::once(error.message)
        .chain(error.details.into_iter().map(|detail| detail.message))
        .collect::<Vec<_>>()
        .join("：");
    let detail = aisix_gateway::upstream_error_scrub::scrub_upstream_error_text(&detail);
    ConnectorError::user(Box::new(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        detail,
    )))
}

fn request_into_reqwest(request: HttpRequest) -> Result<reqwest::Request, ConnectorError> {
    let request = request
        .try_into_http1x()
        .map_err(|error| ConnectorError::user(Box::new(error)))?;
    reqwest::Request::try_from(
        request.map(|body| reqwest::Body::wrap_stream(BodyDataStream::new(body))),
    )
    .map_err(transport_error)
}

fn response_into_smithy(response: reqwest::Response) -> Result<HttpResponse, ConnectorError> {
    let status = response.status();
    let version = response.version();
    let headers = response.headers().clone();
    let stream = response
        .bytes_stream()
        .map_err(|error| std::io::Error::other(safe_transport_detail(&error)))
        .map_ok(http_body::Frame::data);
    let body = SdkBody::from_body_1_x(StreamBody::new(stream));
    let mut response = http::Response::new(body);
    *response.status_mut() = status;
    *response.version_mut() = version;
    *response.headers_mut() = headers;
    HttpResponse::try_from(response).map_err(|error| ConnectorError::other(Box::new(error), None))
}

fn safe_transport_detail(error: &reqwest::Error) -> String {
    // 不让含配置 URL 的 reqwest 原始错误进入 SDK 的 Debug 或日志链。
    aisix_gateway::upstream_error_scrub::scrub_upstream_error_text(
        &aisix_gateway::upstream_http::transport_error_message(error),
    )
}

fn transport_error(error: reqwest::Error) -> ConnectorError {
    let source = Box::new(std::io::Error::other(safe_transport_detail(&error)));
    if error.is_timeout() {
        ConnectorError::timeout(source)
    } else if error.is_builder() {
        ConnectorError::user(source)
    } else {
        ConnectorError::io(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bytes::Bytes;
    use futures::StreamExt;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn credential_requests_check_the_actual_sdk_url_before_dialing() {
        let outbound = Arc::new(
            upstream_http::Client::new(&aisix_core::config::SsrfProtectionConfig::default())
                .expect("默认出站策略"),
        );
        let connector = Connector {
            transport: Transport::Credentials {
                outbound,
                timeout: Duration::from_millis(50),
            },
            read_timeout: Some(Duration::from_secs(1)),
        };
        // 没有监听者的本地端口：删掉发送前的策略检查会变成连接错误，而不是策略拒绝。
        let request =
            HttpRequest::get("http://127.0.0.1:0/actual-sts-endpoint").expect("合法 SDK 请求");
        let error = connector.call(request).await.expect_err("私网必须拒绝");
        assert!(
            error.is_user(),
            "必须在发送前拒绝，而不是连接失败：{error:?}"
        );
        assert!(
            format!("{error:?}").contains("outbound.ssrf_protection.allowed_hosts"),
            "策略拒绝必须保留操作员可修复的原因"
        );
    }

    #[tokio::test]
    async fn credentials_do_not_inherit_a_models_private_network_exception() {
        let transport = Transport::Credentials {
            outbound: Arc::new(
                upstream_http::Client::new(&aisix_core::config::SsrfProtectionConfig {
                    enabled: true,
                    allowed_hosts: vec!["model.example".into()],
                })
                .expect("模型域名白名单"),
            ),
            timeout: Duration::from_millis(50),
        };
        for raw in [
            "http://169.254.169.254/latest/meta-data/",
            "http://localhost:0/token",
            "http://[::1]:0/token",
        ] {
            let url = reqwest::Url::parse(raw).expect("合法实际地址");
            // 仅构建受保护客户端，不发送请求，链路本地地址不会被探测。
            let error = transport
                .client_for(&url)
                .await
                .expect_err("凭据私网地址必须拒绝");
            assert!(error.is_user(), "{raw}: {error:?}");
        }
    }

    #[tokio::test]
    async fn a_credential_endpoint_explicitly_allowed_by_its_own_policy_can_be_used() {
        let transport = Transport::Credentials {
            outbound: Arc::new(
                upstream_http::Client::new(&aisix_core::config::SsrfProtectionConfig {
                    enabled: true,
                    allowed_hosts: vec!["127.0.0.1".into()],
                })
                .expect("凭据地址白名单"),
            ),
            timeout: Duration::from_millis(50),
        };
        let url = reqwest::Url::parse("http://127.0.0.1:0/token").expect("合法实际地址");
        // 白名单只允许构建连接，测试不向本地凭据服务发送请求。
        assert!(transport.client_for(&url).await.is_ok());
    }

    #[tokio::test]
    async fn credential_policy_errors_scrub_userinfo_and_noted_credentials() {
        let tap = Arc::new(aisix_gateway::response_tap::ResponseHeaderTap::new(
            Vec::new(),
            false,
            "credential-policy-test".into(),
        ));
        let secret = "credential-policy-secret-7ce4";
        let password = "configured-userinfo-pass-7ce4";
        let error = aisix_gateway::response_tap::scope(tap, async {
            aisix_gateway::response_tap::note_sent_credential(secret);
            policy_error(ApiError::upstream_unreachable(format!(
                "https://operator:{password}@sts.internal.example/token reported {secret}"
            )))
        })
        .await;
        let printed = format!("{error:?}");
        assert!(
            !printed.contains(secret),
            "登记凭据不能进入 SDK 错误：{printed}"
        );
        assert!(
            !printed.contains(password),
            "URL 密码不能进入 SDK 错误：{printed}"
        );
        assert!(
            !printed.contains("operator:"),
            "URL 用户信息不能进入 SDK 错误：{printed}"
        );
    }

    #[tokio::test]
    async fn the_signed_request_keeps_its_method_url_headers_and_body() {
        let mut request = http::Request::builder()
            .method("POST")
            .uri("https://bedrock-runtime.us-east-1.amazonaws.com/model/a%3Ab/converse-stream?x=1")
            .header("authorization", "AWS4-HMAC-SHA256 Credential=example")
            .header("x-amz-content-sha256", "signed-body-hash")
            .header("content-type", "application/json")
            .body(SdkBody::from("{\"messages\":[]}"))
            .expect("有效 SDK 请求");
        request
            .headers_mut()
            .append("x-test", "first".parse().expect("头值"));
        request
            .headers_mut()
            .append("x-test", "second".parse().expect("头值"));
        let request = HttpRequest::try_from(request).expect("SDK 请求");
        let mut converted = request_into_reqwest(request).expect("请求转换");
        assert_eq!(converted.method(), http::Method::POST);
        assert_eq!(converted.url().path(), "/model/a%3Ab/converse-stream");
        assert_eq!(converted.url().query(), Some("x=1"));
        assert_eq!(
            converted.headers()["authorization"],
            "AWS4-HMAC-SHA256 Credential=example"
        );
        assert_eq!(
            converted.headers()["x-amz-content-sha256"],
            "signed-body-hash"
        );
        assert_eq!(converted.headers().get_all("x-test").iter().count(), 2);
        let body = converted
            .body_mut()
            .take()
            .expect("请求体")
            .collect()
            .await
            .expect("读请求体")
            .to_bytes();
        assert_eq!(body.as_ref(), b"{\"messages\":[]}");
    }

    #[tokio::test]
    async fn response_headers_and_event_bytes_are_forwarded_lazily() {
        let polls = Arc::new(AtomicUsize::new(0));
        let recorded = polls.clone();
        let stream = futures::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from_static(&[0, 0, 0, 12])),
            Ok(Bytes::from_static(&[255, 10, 0, 5])),
        ])
        .inspect(move |_| {
            recorded.fetch_add(1, Ordering::SeqCst);
        });
        let response = http::Response::builder()
            .status(206)
            .header("content-type", "application/vnd.amazon.eventstream")
            .header("x-amzn-requestid", "request-7")
            .body(reqwest::Body::wrap_stream(stream))
            .expect("有效响应");
        let response = response_into_smithy(reqwest::Response::from(response)).expect("响应转换");
        assert_eq!(response.status().as_u16(), 206);
        assert_eq!(
            response.headers().get("x-amzn-requestid"),
            Some("request-7")
        );
        assert_eq!(polls.load(Ordering::SeqCst), 0, "转换不能预读事件流");
        let mut body = BodyDataStream::new(response.into_body());
        assert_eq!(
            body.next().await.expect("首事件").expect("字节"),
            Bytes::from_static(&[0, 0, 0, 12])
        );
        assert_eq!(polls.load(Ordering::SeqCst), 1, "只读取已请求的事件");
        assert_eq!(
            body.next().await.expect("次事件").expect("字节"),
            Bytes::from_static(&[255, 10, 0, 5])
        );
        assert!(body.next().await.is_none());
    }

    #[tokio::test]
    async fn an_event_stream_read_failure_is_not_silently_an_eof() {
        let stream = futures::stream::iter([
            Ok(Bytes::from_static(b"first")),
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "reset",
            )),
        ]);
        let response = http::Response::new(reqwest::Body::wrap_stream(stream));
        let response = response_into_smithy(reqwest::Response::from(response)).expect("响应转换");
        let mut body = BodyDataStream::new(response.into_body());
        assert_eq!(
            body.next().await.expect("首事件").expect("字节"),
            Bytes::from_static(b"first")
        );
        assert!(body.next().await.expect("读错误必须保留").is_err());
    }
}
