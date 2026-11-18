//! 后端集成测试：真实 HTTP peer 返回原始 request-target，断言来自 OpenAPI 规范示例。

use std::sync::Arc;

use aisix_core::mcp_openapi::{check_spec_yields_tools, generate};
use aisix_core::{McpServer, ResourceEntry};
use aisix_mcp::{McpBridge, OpenApiBridge};
use axum::http::Uri;
use serde_json::{json, Value};

async fn wire(parameter: Value, value: Value, expected: &str) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind HTTP peer");
    let address = listener.local_addr().expect("HTTP peer address");
    let peer = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().fallback(|uri: Uri| async move { uri.to_string() }),
        )
        .await
        .expect("serve HTTP peer");
    });
    let path = if parameter["in"] == "path" {
        "/capture/{color}"
    } else {
        "/capture"
    };
    let server: McpServer = serde_json::from_value(json!({
        "name": "wire", "type": "openapi", "url": format!("http://{address}"),
        "spec": { "openapi": "3.1.1", "info": {"title": "wire", "version": "1"},
        "paths": { path: { "get": {
            "operationId": "capture", "parameters": [parameter],
            "responses": {"200": {"description": "captured request target"}}
        } } } }
    }))
    .expect("OpenAPI server");
    let bridge = OpenApiBridge::new(Arc::new(ResourceEntry::new(
        format!("wire-{address}"),
        server,
        1,
    )));
    let tools = bridge.list_tools().await.expect("list generated tools");
    assert_eq!(tools.len(), 1, "supported operation must be reachable");
    let response = bridge
        .call_tool("capture", json!({ "color": value }))
        .await
        .expect("execute tool");
    peer.abort();
    assert!(!response.is_error, "{response:?}");
    assert_eq!(response.content[0]["text"], expected);
}

fn parameter(location: &str, schema: Value, explode: Option<bool>) -> Value {
    let mut parameter = json!({
        "name": "color", "in": location, "required": true, "schema": schema
    });
    if let Some(explode) = explode {
        parameter["explode"] = json!(explode);
    }
    parameter
}

#[tokio::test]
async fn simple_path_object_defaults_to_flat_key_value_pairs() {
    wire(
        parameter("path", json!({ "type": "object" }), None),
        json!({ "first": "a,b", "second": "x=y" }),
        "/capture/first,a%2Cb,second,x%3Dy",
    )
    .await;
}

#[tokio::test]
async fn simple_path_object_explode_keeps_structural_equals_unescaped() {
    wire(
        parameter("path", json!({ "type": "object" }), Some(true)),
        json!({ "first": "a,b", "second": "x=y" }),
        "/capture/first=a%2Cb,second=x%3Dy",
    )
    .await;
}

#[tokio::test]
async fn form_query_object_defaults_to_exploded_members() {
    wire(
        parameter("query", json!({ "type": "object" }), None),
        json!({ "first": "a,b", "second": "x=y" }),
        "/capture?first=a%2Cb&second=x%3Dy",
    )
    .await;
}

#[tokio::test]
async fn form_query_object_without_explode_keeps_commas_as_delimiters() {
    wire(
        parameter("query", json!({ "type": "object" }), Some(false)),
        json!({ "first": "a,b", "second": "x=y" }),
        "/capture?color=first,a%2Cb,second,x%3Dy",
    )
    .await;
}

#[tokio::test]
async fn form_query_array_without_explode_is_one_delimited_value() {
    wire(
        parameter(
            "query",
            json!({ "type": "array", "items": {"type": "string"} }),
            Some(false),
        ),
        json!(["a,b", "x=y"]),
        "/capture?color=a%2Cb,x%3Dy",
    )
    .await;
}

#[tokio::test]
async fn existing_default_array_styles_stay_callable() {
    wire(
        parameter(
            "path",
            json!({ "type": "array", "items": {"type": "string"} }),
            None,
        ),
        json!(["a,b", "x=y"]),
        "/capture/a%2Cb,x%3Dy",
    )
    .await;
    wire(
        parameter(
            "query",
            json!({ "type": "array", "items": {"type": "string"} }),
            None,
        ),
        json!(["a,b", "x=y"]),
        "/capture?color=a%2Cb&color=x%3Dy",
    )
    .await;
}

#[test]
fn unsupported_styles_are_not_published_as_callable_tools() {
    for (location, style) in [
        ("path", "label"),
        ("path", "matrix"),
        ("query", "spaceDelimited"),
        ("query", "pipeDelimited"),
        ("query", "deepObject"),
    ] {
        let path = if location == "path" {
            "/capture/{color}"
        } else {
            "/capture"
        };
        let spec = json!({"paths": {path: {"get": {
            "operationId": "unsupported", "parameters": [{
                "name": "color", "in": location, "style": style,
                "required": true, "schema": {"type": "object"}
            }]
        }}}});
        let generated = generate(&spec).expect("walk spec");
        assert!(generated.tools.is_empty(), "published {location} {style}");
        let error = check_spec_yields_tools(&spec).expect_err("reject an unusable spec");
        assert!(error.contains(style), "missing unsupported style: {error}");

        // 一个不支持的 operation 不能使同文档中的正常工具消失。
        let mut mixed = spec.clone();
        mixed["paths"]["/supported"] = json!({"get": {"operationId": "supported"}});
        let generated = generate(&mixed).expect("walk mixed spec");
        let names: Vec<_> = generated
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(names, ["supported"]);
        check_spec_yields_tools(&mixed).expect("the supported neighbor keeps the spec usable");
    }
}

#[test]
fn operation_parameter_override_controls_style_before_generation() {
    let spec = json!({"paths": {"/capture/{color}": {
        "parameters": [{"name": "color", "in": "path", "style": "label", "schema": {"type": "string"}}],
        "get": {"operationId": "capture", "parameters": [{
            "name": "color", "in": "path", "style": "simple", "required": true,
            "schema": {"type": "string"}
        }]}
    }}});
    let generated = generate(&spec).expect("walk spec");
    assert_eq!(generated.tools.len(), 1);
    assert!(generated.skipped.is_empty());
}
