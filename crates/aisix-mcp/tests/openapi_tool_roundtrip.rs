//! End-to-end test of an OpenAPI-backed server behind the MCP gateway: a real
//! REST API (axum, ephemeral port) is registered as a `type: openapi`
//! `mcp_server`, and a real rmcp client drives the gateway's `/mcp` endpoint.
//!
//! Pins the issue-level acceptance criteria at the crate boundary:
//! - `tools/list` exposes one namespaced tool per spec operation with the
//!   generated input schema;
//! - `tools/call` executes the REST request — path substitution, query
//!   parameters, JSON body, and the gateway-held credential (never supplied
//!   by the agent);
//! - a non-2xx response and an argument mistake surface as tool-level errors
//!   (`isError: true`), not protocol errors.

use std::collections::HashMap;
use std::net::SocketAddr;

use aisix_core::{AisixSnapshot, McpServer, ResourceEntry};
use aisix_mcp::{streamable_http_service, McpGateway};
use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;
use serde_json::{json, Value};

/// The bearer token the gateway holds for the fake ERP API. The REST handlers
/// 401 without it, proving the credential is injected gateway-side.
const ERP_TOKEN: &str = "tok-erp-123";

/// The custom API-key header (and key) for the second, `api_key`-mode server.
const INVENTORY_HEADER: &str = "x-inventory-key";
const INVENTORY_KEY: &str = "inv-key-456";

async fn serve(app: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    addr
}

/// A fake ERP REST API: bearer-authenticated echo endpoints.
async fn spawn_erp_api() -> SocketAddr {
    fn authed(headers: &HeaderMap) -> bool {
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == format!("Bearer {ERP_TOKEN}"))
    }

    let app = axum::Router::new()
        .route(
            "/v1/items/:id",
            get(
                |headers: HeaderMap,
                 Path(id): Path<String>,
                 Query(q): Query<HashMap<String, String>>| async move {
                    if !authed(&headers) {
                        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "no auth"})))
                            .into_response();
                    }
                    Json(json!({ "id": id, "query": q })).into_response()
                },
            ),
        )
        .route(
            "/v1/orders",
            post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                if !authed(&headers) {
                    return (StatusCode::UNAUTHORIZED, Json(json!({"error": "no auth"})))
                        .into_response();
                }
                Json(json!({ "created": body })).into_response()
            }),
        )
        .route(
            "/v1/fail",
            // Quotes the credential it was sent, as some APIs do in an error.
            get(|headers: HeaderMap| async move {
                let saw = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("boom; saw {saw}"),
                )
            }),
        );
    serve(app).await
}

/// The ERP API's OpenAPI document, as the control plane would materialize it.
fn erp_spec() -> Value {
    json!({
        "openapi": "3.0.0",
        "info": { "title": "ERP", "version": "1.0.0" },
        "paths": {
            "/items/{id}": {
                "get": {
                    "operationId": "getItem",
                    "summary": "Fetch one item",
                    "parameters": [
                        { "name": "id", "in": "path", "required": true,
                          "schema": { "type": "integer" } },
                        { "name": "verbose", "in": "query",
                          "schema": { "type": "boolean" } }
                    ]
                }
            },
            "/orders": {
                "post": {
                    "operationId": "createOrder",
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": {
                            "type": "object",
                            "properties": { "note": { "type": "string" } },
                            "required": ["note"]
                        } } }
                    }
                }
            },
            "/fail": { "get": { "operationId": "failOp" } }
        }
    })
}

fn openapi_entry(id: &str, config: Value) -> ResourceEntry<McpServer> {
    let server: McpServer = serde_json::from_value(config).expect("valid mcp_server resource");
    ResourceEntry::new(id, server, 1)
}

async fn spawn_gateway(gateway: McpGateway) -> SocketAddr {
    serve(axum::Router::new().nest_service("/mcp", streamable_http_service(gateway, 0))).await
}

fn first_text(result: &CallToolResult) -> String {
    let value = serde_json::to_value(&result.content).expect("encode content");
    value[0]["text"].as_str().unwrap_or_default().to_string()
}

fn call(name: &str, args: Value) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(name.to_string());
    if let Value::Object(map) = args {
        params = params.with_arguments(map);
    }
    params
}

#[tokio::test]
async fn openapi_server_lists_and_calls_generated_tools() {
    let api = spawn_erp_api().await;

    let snapshot = AisixSnapshot::new();
    snapshot.mcp_servers.insert(openapi_entry(
        "e1",
        json!({
            "name": "erp",
            "type": "openapi",
            "url": format!("http://{api}/v1"),
            "spec": erp_spec(),
            "auth_type": "bearer",
            "secret": ERP_TOKEN,
        }),
    ));

    let gw = spawn_gateway(McpGateway::from_snapshot(&snapshot)).await;
    let client = ()
        .serve(StreamableHttpClientTransport::from_uri(format!(
            "http://{gw}/mcp"
        )))
        .await
        .expect("connect downstream client");

    // tools/list: one namespaced tool per operation, schema preserved.
    let tools = client.list_tools(None).await.expect("list tools");
    let mut names: Vec<_> = tools.tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["erp__createorder", "erp__failop", "erp__getitem"]
    );

    let get_item = tools
        .tools
        .iter()
        .find(|t| t.name == "erp__getitem")
        .expect("getItem generated");
    let schema = serde_json::to_value(&get_item.input_schema).expect("schema");
    assert_eq!(schema["properties"]["id"]["type"], "integer");
    assert_eq!(schema["properties"]["verbose"]["type"], "boolean");
    assert_eq!(schema["required"], json!(["id"]));

    // tools/call GET: path substitution + query serialization + gateway-held
    // bearer (the client never sent a credential).
    let result = client
        .call_tool(call("erp__getitem", json!({ "id": 42, "verbose": true })))
        .await
        .expect("call getItem");
    assert_ne!(result.is_error, Some(true), "unexpected tool error");
    let echoed: Value = serde_json::from_str(&first_text(&result)).expect("json echo");
    assert_eq!(echoed["id"], "42");
    assert_eq!(echoed["query"]["verbose"], "true");

    // tools/call POST: the `body` argument becomes the JSON request body.
    let result = client
        .call_tool(call(
            "erp__createorder",
            json!({ "body": { "note": "hello" } }),
        ))
        .await
        .expect("call createOrder");
    assert_ne!(result.is_error, Some(true));
    let echoed: Value = serde_json::from_str(&first_text(&result)).expect("json echo");
    assert_eq!(echoed["created"]["note"], "hello");

    // Non-2xx → tool-level error carrying the status and body.
    let result = client
        .call_tool(call("erp__failop", Value::Null))
        .await
        .expect("call failOp");
    assert_eq!(result.is_error, Some(true));
    let text = first_text(&result);
    assert!(text.starts_with("HTTP 500:"), "got: {text}");
    assert!(text.contains("boom"), "got: {text}");
    assert!(
        !text.contains(ERP_TOKEN),
        "the gateway's credential reached the agent: {text}"
    );

    // Argument mistake (missing required path param) → tool-level error the
    // agent can read and fix, not an opaque protocol error.
    let result = client
        .call_tool(call("erp__getitem", json!({ "verbose": true })))
        .await
        .expect("call getItem without id");
    assert_eq!(result.is_error, Some(true));
    assert!(
        first_text(&result).contains("missing required path parameter 'id'"),
        "got: {}",
        first_text(&result)
    );

    client.cancel().await.ok();
}

#[tokio::test]
async fn openapi_server_sends_api_key_under_configured_header() {
    // Echo back the configured custom header so the assertion sees exactly
    // what the gateway sent.
    let app = axum::Router::new().route(
        "/lookup",
        get(|headers: HeaderMap| async move {
            let key = headers
                .get(INVENTORY_HEADER)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            Json(json!({ "received_key": key }))
        }),
    );
    let api = serve(app).await;

    let snapshot = AisixSnapshot::new();
    snapshot.mcp_servers.insert(openapi_entry(
        "e2",
        json!({
            "name": "inventory",
            "type": "openapi",
            "url": format!("http://{api}"),
            "spec": {
                "openapi": "3.0.0",
                "paths": { "/lookup": { "get": { "operationId": "lookup" } } }
            },
            "auth_type": "api_key",
            "secret": INVENTORY_KEY,
            "api_key_header": INVENTORY_HEADER,
        }),
    ));

    let gw = spawn_gateway(McpGateway::from_snapshot(&snapshot)).await;
    let client = ()
        .serve(StreamableHttpClientTransport::from_uri(format!(
            "http://{gw}/mcp"
        )))
        .await
        .expect("connect downstream client");

    let result = client
        .call_tool(call("inventory__lookup", Value::Null))
        .await
        .expect("call lookup");
    assert_ne!(result.is_error, Some(true));
    let echoed: Value = serde_json::from_str(&first_text(&result)).expect("json echo");
    assert_eq!(echoed["received_key"], INVENTORY_KEY);

    client.cancel().await.ok();
}

#[tokio::test]
async fn broken_spec_degrades_gracefully_next_to_healthy_servers() {
    // One healthy openapi server and one whose spec is unusable: the broken
    // one's tools are absent, the healthy one keeps serving — mirroring how a
    // dead real upstream degrades.
    let api = spawn_erp_api().await;

    let snapshot = AisixSnapshot::new();
    snapshot.mcp_servers.insert(openapi_entry(
        "e1",
        json!({
            "name": "erp",
            "type": "openapi",
            "url": format!("http://{api}/v1"),
            "spec": erp_spec(),
            "auth_type": "bearer",
            "secret": ERP_TOKEN,
        }),
    ));
    snapshot.mcp_servers.insert(openapi_entry(
        "e2",
        json!({
            "name": "broken",
            "type": "openapi",
            "url": "http://127.0.0.1:1/api",
            "spec": { "openapi": "3.0.0" }
        }),
    ));

    let gw = spawn_gateway(McpGateway::from_snapshot(&snapshot)).await;
    let client = ()
        .serve(StreamableHttpClientTransport::from_uri(format!(
            "http://{gw}/mcp"
        )))
        .await
        .expect("connect downstream client");

    let tools = client.list_tools(None).await.expect("list tools");
    let names: Vec<_> = tools.tools.iter().map(|t| t.name.to_string()).collect();
    assert!(names.iter().all(|n| n.starts_with("erp__")), "{names:?}");
    assert_eq!(names.len(), 3);

    client.cancel().await.ok();
}

/// A fake internal system server that reports back the headers it
/// received — the P4-04 shape: a REST API registered as tools, authorizing
/// on the caller's own credential rather than on the gateway's.
async fn spawn_claims_api() -> SocketAddr {
    let app = axum::Router::new().route(
        "/v1/whoami",
        get(|headers: HeaderMap| async move {
            let jwt = headers
                .get("x-user-jwt")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let authorizations: Vec<String> = headers
                .get_all("authorization")
                .iter()
                .filter_map(|v| v.to_str().ok().map(str::to_string))
                .collect();
            let session = headers
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            Json(json!({
                "user_jwt": jwt,
                "authorizations": authorizations,
                "mcp_session_id": session,
            }))
            .into_response()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    addr
}

fn whoami_spec() -> Value {
    json!({
        "openapi": "3.0.0",
        "info": { "title": "claims", "version": "1" },
        "paths": {
            "/whoami": {
                "get": { "operationId": "whoami", "responses": { "200": { "description": "ok" } } }
            }
        }
    })
}

/// Drive a tool call through the gateway with `client` as the inbound
/// request's headers, and read back what the REST API actually received.
async fn whoami_via_gateway(config: Value, client: &[(&str, &str)]) -> Value {
    let snapshot = AisixSnapshot::new();
    snapshot.mcp_servers.insert(openapi_entry("claims", config));

    let mut inbound = http::HeaderMap::new();
    for (k, v) in client {
        inbound.insert(
            http::HeaderName::try_from(*k).expect("header name"),
            http::HeaderValue::from_str(v).expect("header value"),
        );
    }
    let gw = spawn_gateway(McpGateway::from_snapshot_for_request(
        &snapshot,
        Some(&inbound),
        &aisix_mcp::CallerCredentials::default(),
        aisix_mcp::ToolAcl::from_allowed(Some(&["*".to_string()])),
        "",
    ))
    .await;
    let client = ()
        .serve(StreamableHttpClientTransport::from_uri(format!(
            "http://{gw}/mcp"
        )))
        .await
        .expect("connect downstream client");
    let result = client
        .call_tool(call("claims__whoami", Value::Null))
        .await
        .expect("call whoami");
    assert_ne!(result.is_error, Some(true), "unexpected tool error");
    serde_json::from_str(&first_text(&result)).expect("whoami returns JSON")
}

#[tokio::test]
async fn a_forwarded_client_header_reaches_a_rest_api_exposed_as_tools() {
    let api = spawn_claims_api().await;
    let seen = whoami_via_gateway(
        json!({
            "name": "claims",
            "type": "openapi",
            "url": format!("http://{api}/v1"),
            "spec": whoami_spec(),
            "forward_client_headers": ["x-user-jwt"],
        }),
        &[("x-user-jwt", "eyJhbGciOi.caller")],
    )
    .await;
    assert_eq!(seen["user_jwt"], "eyJhbGciOi.caller");
}

#[tokio::test]
async fn a_forwarded_header_replaces_the_gateway_credential_in_the_same_slot() {
    let api = spawn_claims_api().await;
    let seen = whoami_via_gateway(
        json!({
            "name": "claims",
            "type": "openapi",
            "url": format!("http://{api}/v1"),
            "spec": whoami_spec(),
            "auth_type": "bearer",
            "secret": "gateway-held-secret",
            "forward_client_headers": ["authorization"],
        }),
        &[("authorization", "Bearer eyJhbGciOi.caller")],
    )
    .await;
    assert_eq!(
        seen["authorizations"],
        json!(["Bearer eyJhbGciOi.caller"]),
        "the caller's credential replaces the gateway's, and rides alone"
    );
}

#[tokio::test]
async fn a_caller_that_sent_nothing_keeps_the_gateway_credential() {
    let api = spawn_claims_api().await;
    let seen = whoami_via_gateway(
        json!({
            "name": "claims",
            "type": "openapi",
            "url": format!("http://{api}/v1"),
            "spec": whoami_spec(),
            "auth_type": "bearer",
            "secret": "gateway-held-secret",
            "forward_client_headers": ["authorization"],
        }),
        &[],
    )
    .await;
    assert_eq!(seen["user_jwt"], "");
    assert_eq!(
        seen["authorizations"],
        json!(["Bearer gateway-held-secret"]),
        "an opted-in slot the caller left empty keeps the gateway's own credential"
    );
}

#[tokio::test]
async fn a_server_without_the_field_forwards_nothing() {
    let api = spawn_claims_api().await;
    let seen = whoami_via_gateway(
        json!({
            "name": "claims",
            "type": "openapi",
            "url": format!("http://{api}/v1"),
            "spec": whoami_spec(),
        }),
        &[
            ("x-user-jwt", "eyJhbGciOi.caller"),
            ("authorization", "Bearer eyJhbGciOi.caller"),
        ],
    )
    .await;
    assert_eq!(seen["user_jwt"], "");
    assert_eq!(seen["authorizations"], json!([]));
}

#[tokio::test]
async fn the_mcp_session_slots_are_never_forwarded() {
    let api = spawn_claims_api().await;
    // The caller's own session with THIS gateway. Forwarding it names a
    // session the upstream never opened, and an MCP upstream rejects a
    // foreign value outright — so no pattern reaches these.
    let seen = whoami_via_gateway(
        json!({
            "name": "claims",
            "type": "openapi",
            "url": format!("http://{api}/v1"),
            "spec": whoami_spec(),
            "forward_client_headers": ["*"],
        }),
        &[("mcp-session-id", "callers-own-session")],
    )
    .await;
    assert_eq!(seen["mcp_session_id"], "");
}

/// A base URL's userinfo goes out as `Authorization: Basic`, and a REST API
/// that quotes the headers it saw must not hand that credential to the agent,
/// encoded or not.
#[tokio::test]
async fn a_rest_api_quoting_the_basic_credential_of_its_url_does_not_reach_the_agent() {
    use base64::Engine as _;
    let api = serve(axum::Router::new().route(
        "/v1/fail",
        get(|headers: HeaderMap| async move {
            let saw = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            (
                StatusCode::UNAUTHORIZED,
                format!("rejected credentials: {saw} (password S3cretPassw0rd99)"),
            )
        }),
    ))
    .await;
    let snapshot = AisixSnapshot::new();
    let server: McpServer = serde_json::from_value(json!({
        "name": "erp",
        "type": "openapi",
        "url": format!("http://svc-user:S3cretPassw0rd99@{api}/v1"),
        "spec": {
            "openapi": "3.0.0",
            "info": { "title": "ERP", "version": "1.0.0" },
            "paths": { "/fail": { "get": { "operationId": "failOp" } } }
        },
        "auth_type": "none",
    }))
    .unwrap();
    snapshot
        .mcp_servers
        .insert(ResourceEntry::new("e1", server, 1));
    let gw = spawn_gateway(McpGateway::from_snapshot(&snapshot)).await;
    let client = ()
        .serve(StreamableHttpClientTransport::from_uri(format!(
            "http://{gw}/mcp"
        )))
        .await
        .unwrap();
    let result = client
        .call_tool(call("erp__failop", Value::Null))
        .await
        .expect("call failOp");
    let text = first_text(&result);
    let basic = base64::engine::general_purpose::STANDARD.encode("svc-user:S3cretPassw0rd99");
    assert!(text.contains("rejected credentials"), "got: {text}");
    assert!(
        !text.contains(&basic),
        "the Basic credential reached the agent: {text}"
    );
    assert!(
        !text.contains("S3cretPassw0rd99"),
        "the password reached the agent: {text}"
    );
    client.cancel().await.ok();
}

/// A REST API that answers every request with what it received — method,
/// path, raw query, `Content-Type` and raw body — so a test reads exactly
/// what a generated tool put on the wire. Under `/strict` it refuses any
/// media type but the one its spec declares, with `415`, as a conforming
/// server may.
async fn spawn_capture_api() -> SocketAddr {
    async fn capture(
        method: axum::http::Method,
        uri: axum::http::Uri,
        headers: HeaderMap,
        body: axum::body::Bytes,
    ) -> axum::response::Response {
        let content_type = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if uri.path().starts_with("/api/strict")
            && content_type.as_deref() != Some("application/vnd.api+json")
        {
            return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported media type").into_response();
        }
        Json(json!({
            "method": method.as_str(),
            "path": uri.path(),
            "query": uri.query(),
            "content_type": content_type,
            "body": String::from_utf8_lossy(&body),
        }))
        .into_response()
    }
    serve(axum::Router::new().fallback(capture)).await
}

/// One operation per body/parameter shape the generator publishes.
fn capture_spec() -> Value {
    let json_body = |schema: Value| json!({ "required": true, "content": { "application/json": { "schema": schema } } });
    json!({
        "openapi": "3.0.3",
        "info": { "title": "capture", "version": "1" },
        "paths": {
            "/count": { "post": { "operationId": "count",
                "requestBody": json_body(json!({ "type": "integer" })) } },
            "/flag": { "post": { "operationId": "flag",
                "requestBody": json_body(json!({ "type": "boolean" })) } },
            "/label": { "post": { "operationId": "label",
                "requestBody": json_body(json!({ "type": "string" })) } },
            "/maybe": { "post": { "operationId": "maybe",
                "requestBody": { "content": { "application/json": {
                    "schema": { "type": "integer", "nullable": true } } } } } },
            "/note": { "post": { "operationId": "note",
                "requestBody": json_body(json!({ "type": "object" })) } },
            "/choice": { "post": { "operationId": "choice",
                "requestBody": json_body(json!({ "oneOf": [
                    { "type": "integer" }, { "type": "string" } ] })) } },
            "/tier": { "post": { "operationId": "tier",
                "requestBody": json_body(json!({ "enum": [7, 8] })) } },
            "/maybe-choice": { "post": { "operationId": "maybeChoiceCasing",
                "requestBody": { "content": { "application/json": {
                    "schema": { "oneOf": [
                        { "type": "integer" }, { "type": "null" } ] } } } } } },
            "/things/{id}/{body}": { "post": {
                "operationId": "things",
                "parameters": [
                    { "name": "id", "in": "path", "required": true, "schema": { "type": "integer" } },
                    { "name": "id", "in": "query", "schema": { "type": "string" } },
                    { "name": "body", "in": "path", "required": true, "schema": { "type": "string" } }
                ],
                "requestBody": json_body(json!({ "type": "object" }))
            } },
            "/batch/{ids}": { "get": {
                "operationId": "batch",
                "parameters": [ { "name": "ids", "in": "path", "required": true,
                    "schema": { "type": "array", "items": { "type": "string" } } } ]
            } },
            "/strict": { "post": { "operationId": "strict",
                "requestBody": { "required": true, "content": { "application/vnd.api+json": {
                    "schema": { "type": "object" } } } } } },
            "/either": { "post": { "operationId": "either",
                "requestBody": { "required": true, "content": {
                    "application/vnd.api+json": { "schema": { "type": "object" } },
                    "application/json": { "schema": { "type": "object" } } } } } }
        }
    })
}

/// A client connected to a gateway serving [`capture_spec`] against the
/// capture API, and a `tools/call` that returns what the API received.
async fn capture_client(
) -> impl Fn(&'static str, Value) -> futures::future::BoxFuture<'static, (Option<bool>, Value, Value)>
{
    let api = spawn_capture_api().await;
    let snapshot = AisixSnapshot::new();
    snapshot.mcp_servers.insert(openapi_entry(
        "capture",
        json!({
            "name": "cap",
            "type": "openapi",
            "url": format!("http://{api}/api"),
            "spec": capture_spec(),
        }),
    ));
    let gw = spawn_gateway(McpGateway::from_snapshot(&snapshot)).await;
    let client = std::sync::Arc::new(
        ().serve(StreamableHttpClientTransport::from_uri(format!(
            "http://{gw}/mcp"
        )))
        .await
        .expect("connect downstream client"),
    );
    let tools = client.list_tools(None).await.expect("list tools").tools;
    let schemas: HashMap<String, Value> = tools
        .into_iter()
        .map(|t| {
            (
                t.name.to_string(),
                serde_json::to_value(&t.input_schema).unwrap(),
            )
        })
        .collect();
    move |name: &'static str, args: Value| {
        let client = client.clone();
        let schema = schemas[&format!("cap__{name}")].clone();
        Box::pin(async move {
            let result = client
                .call_tool(call(&format!("cap__{name}"), args))
                .await
                .expect("tools/call");
            let seen =
                serde_json::from_str(&first_text(&result)).unwrap_or(json!(first_text(&result)));
            (result.is_error, schema, seen)
        })
    }
}

/// A body schema that declares a primitive is sent the primitive the caller
/// gave, as JSON; an explicit `null` reaches the API where the schema is
/// nullable, and an omitted one sends no body.
#[tokio::test]
async fn a_primitive_body_reaches_the_api_as_published() {
    let call = capture_client().await;
    for (tool, value, wire) in [
        ("count", json!(42), "42"),
        ("flag", json!(true), "true"),
        ("label", json!("plain text"), "\"plain text\""),
        ("label", json!("{\"a\":1}"), "\"{\\\"a\\\":1}\""),
        ("maybe", Value::Null, "null"),
        ("note", json!({ "a": 1 }), "{\"a\":1}"),
    ] {
        let (is_error, schema, seen) = call(tool, json!({ "body": value })).await;
        assert_ne!(is_error, Some(true), "{tool}: {seen}");
        // The value sent is one the published schema admits.
        let declared = &schema["properties"]["body"]["type"];
        assert!(declared.is_string(), "{tool}: {schema}");
        assert_eq!(seen["body"], wire, "{tool} sent {value}");
        assert_eq!(seen["content_type"], "application/json", "{tool}");
    }
    let (_, _, seen) = call("maybe", json!({})).await;
    assert_eq!(seen["body"], "", "an omitted optional body sends none");
}

/// A body schema with no top-level `type` — a composition (`oneOf`) or an
/// `enum` — is published as that composition, so what it admits reaches the
/// API unchanged, the same as a declared primitive does; and a composed
/// nullable schema still tells an explicit `null` from no body at all.
#[tokio::test]
async fn a_composed_body_reaches_the_api_as_published() {
    let call = capture_client().await;
    for (tool, value, wire) in [
        ("choice", json!(7), "7"),
        ("choice", json!("text"), "\"text\""),
        ("tier", json!(7), "7"),
        ("maybechoicecasing", Value::Null, "null"),
    ] {
        let (is_error, schema, seen) = call(tool, json!({ "body": value })).await;
        assert_ne!(is_error, Some(true), "{tool}: {seen}");
        let body_schema = &schema["properties"]["body"];
        assert!(
            body_schema.get("type").is_none(),
            "{tool} must publish its composition: {schema}"
        );
        assert_eq!(seen["body"], wire, "{tool} sent {value} as {seen}");
    }
    let (_, _, seen) = call("choice", json!({})).await;
    assert_eq!(
        seen["body"], "",
        "an omitted composed body sends none, not a null"
    );
}

/// Two parameters that share a name, and a path parameter named like the
/// body, are each published under an argument of their own, and each value
/// reaches its own place in the request.
#[tokio::test]
async fn parameters_sharing_a_name_reach_their_own_locations() {
    let call = capture_client().await;
    let (is_error, schema, seen) = call(
        "things",
        json!({ "id__path": 7, "id__query": "q-7", "body__path": "segment", "body": { "k": "v" } }),
    )
    .await;
    assert_ne!(is_error, Some(true), "{seen}");
    let mut published: Vec<String> = schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    published.sort();
    assert_eq!(published, ["body", "body__path", "id__path", "id__query"]);
    assert_eq!(seen["path"], "/api/things/7/segment");
    assert_eq!(seen["query"], "id=q-7");
    assert_eq!(seen["body"], "{\"k\":\"v\"}");
}

/// An array path parameter is written in the default `simple` style: the
/// items comma-separated, each encoded on its own.
#[tokio::test]
async fn an_array_path_parameter_is_written_in_simple_style() {
    let call = capture_client().await;
    let (is_error, schema, seen) = call("batch", json!({ "ids": ["a", "b c", "d,e"] })).await;
    assert_ne!(is_error, Some(true), "{seen}");
    assert_eq!(schema["properties"]["ids"]["type"], "array");
    assert_eq!(seen["path"], "/api/batch/a,b%20c,d%2Ce");
    // An item that would change the request target is still refused.
    let (is_error, _, seen) = call("batch", json!({ "ids": ["a", "../x"] })).await;
    assert_eq!(is_error, Some(true), "{seen}");
}

/// A body published from a vendor JSON media type is sent under it, so an API
/// that insists on its declared type accepts the call; an operation that
/// also offers plain JSON is sent plain JSON.
#[tokio::test]
async fn a_vendor_json_body_is_sent_under_its_own_media_type() {
    let call = capture_client().await;
    let (is_error, _, seen) = call("strict", json!({ "body": { "data": { "type": "x" } } })).await;
    assert_ne!(is_error, Some(true), "{seen}");
    assert_eq!(seen["content_type"], "application/vnd.api+json");
    assert_eq!(seen["body"], "{\"data\":{\"type\":\"x\"}}");
    let (_, _, seen) = call("either", json!({ "body": {} })).await;
    assert_eq!(seen["content_type"], "application/json");
}

/// A REST response is read up to a fixed cap: one past it fails the call
/// whole, whether its length is declared or it streams without one, rather
/// than being held in memory; a small answer from the same API is
/// unaffected. Driven on the bridge itself, because a downstream MCP client
/// caps what it reads too and would hide whether the gateway buffered it.
#[tokio::test]
async fn a_response_past_the_cap_fails_the_call_instead_of_being_buffered() {
    use aisix_mcp::{McpBridge, McpError, OpenApiBridge};
    const PAST_CAP: usize = 17 * 1024 * 1024;
    let app = axum::Router::new()
        .route("/declared", get(|| async { "x".repeat(PAST_CAP) }))
        .route(
            "/streamed",
            get(|| async {
                let chunk = axum::body::Bytes::from(vec![b'x'; 1024 * 1024]);
                let chunks = futures::stream::iter(
                    (0..17).map(move |_| Ok::<_, std::convert::Infallible>(chunk.clone())),
                );
                axum::body::Body::from_stream(chunks)
            }),
        )
        .route(
            "/failing",
            get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "x".repeat(PAST_CAP)) }),
        )
        .route("/small", get(|| async { "small answer" }));
    let api = serve(app).await;
    let bridge = OpenApiBridge::new(std::sync::Arc::new(openapi_entry(
        "big",
        json!({
            "name": "big",
            "type": "openapi",
            "url": format!("http://{api}"),
            "spec": { "openapi": "3.0.0", "paths": {
                "/declared": { "get": { "operationId": "declared" } },
                "/streamed": { "get": { "operationId": "streamed" } },
                "/failing": { "get": { "operationId": "failing" } },
                "/small": { "get": { "operationId": "small" } }
            } },
        }),
    )));

    for tool in ["declared", "streamed", "failing"] {
        match bridge.call_tool(tool, Value::Null).await {
            Err(McpError::Request(message)) => {
                assert!(message.contains("limit"), "{tool}: {message}")
            }
            Err(other) => panic!("{tool}: {other}"),
            Ok(result) => panic!(
                "{tool}: a response past the cap was buffered into a {}-byte result",
                result.content.to_string().len()
            ),
        }
    }
    let result = bridge
        .call_tool("small", Value::Null)
        .await
        .expect("a small answer is served");
    assert_eq!(result.content[0]["text"], "small answer");
}
