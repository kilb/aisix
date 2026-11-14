//! 原生 Responses 路由必须先认证；未注册的路由会返回 404，使本测试失败。

use std::sync::Arc;

use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ProxyConfig};
use aisix_gateway::Hub;
use aisix_proxy::{build_router, ProxyState};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn native_responses_routes_authenticate_before_dispatch() {
    let config = ProxyConfig {
        addr: "127.0.0.1:0".into(),
        request_body_limit_bytes: 1024 * 1024,
        real_ip: Default::default(),
        request_id: Default::default(),
        url_rewrites: Vec::new(),
        expose_debug_headers: false,
        platform: Default::default(),
        tls: None,
        listeners: Vec::new(),
        thread_per_core: None,
        workers: None,
    };
    let app = build_router(ProxyState::new(
        SnapshotHandle::new(AisixSnapshot::new()),
        Arc::new(Hub::new()),
        &config,
    ));
    for (method, path) in [
        ("GET", "/v1/responses/resp_owned"),
        ("DELETE", "/v1/responses/resp_owned"),
        ("POST", "/v1/responses/resp_owned/cancel"),
        ("GET", "/v1/responses/resp_owned/input_items"),
        ("POST", "/v1/responses/compact"),
        ("POST", "/v1/responses/input_tokens"),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path}"
        );
    }
}
