use std::net::SocketAddr;
use std::sync::Arc;

use aisix_core::{AisixSnapshot, McpServer, ResourceEntry};
use aisix_mcp::{
    streamable_http_service, EphemeralBridge, McpBridge, McpError, McpGateway, McpUpstream,
    RmcpBridge, ToolAcl,
};
use axum::{body::Bytes, routing::post, Json, Router};
use rmcp::model::{
    CallToolRequestParams, ErrorData, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleServer, ServerHandler, ServiceError, ServiceExt};
use serde_json::{json, Value};

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("test address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve test peer") });
    addr
}

async fn gateway(gateway: McpGateway) -> SocketAddr {
    serve(Router::new().nest_service("/mcp", streamable_http_service(gateway, 0))).await
}

// 删除原值转发或恢复 schema 猜测/coercion 都应使 HTTP wire 断言变红。
#[tokio::test]
async fn constraint_only_body_schemas_preserve_the_callers_json() {
    let api = serve(Router::new().fallback(post(|body: Bytes| async move {
        Json(json!({"wire": String::from_utf8_lossy(&body)}))
    })))
    .await;
    let cases = [
        ("bareallof", json!({"allOf":[{"minimum":0}]})),
        ("bareanyof", json!({"anyOf":[{}]})),
        (
            "bareoneof",
            json!({"oneOf":[{"minimum":0},{"type":"string"}]}),
        ),
        (
            "typedallof",
            json!({"allOf":[{"type":"number"},{"minimum":0}]}),
        ),
        ("integer", json!({"type":"integer"})),
        ("enum", json!({"enum":[7,8]})),
        (
            "typedoneof",
            json!({"oneOf":[{"type":"integer"},{"type":"string"}]}),
        ),
    ];
    let mut paths = serde_json::Map::new();
    for (name, schema) in &cases {
        paths.insert(
            format!("/{name}"),
            json!({"post":{"operationId":name,
            "requestBody":{"content":{"application/json":{"schema":schema}}}}}),
        );
    }
    paths.insert(
        "/nullable".into(),
        json!({"post":{"operationId":"nullable",
        "requestBody":{"content":{"application/json":{"schema":{"type":"null"}}}}}}),
    );
    let config = json!({"name":"capture","type":"openapi","url":format!("http://{api}"),
        "spec":{"openapi":"3.1.0","info":{"title":"capture","version":"1"},"paths":paths}});
    aisix_core::validate_mcp_server(&config).expect("strict admission");
    let server: McpServer = serde_json::from_value(config).expect("resource");
    server.validate_semantics().expect("semantic admission");
    let snapshot = AisixSnapshot::new();
    snapshot
        .mcp_servers
        .insert(ResourceEntry::new("capture", server, 1));
    let addr = gateway(McpGateway::from_snapshot(&snapshot)).await;
    let client = ()
        .serve(StreamableHttpClientTransport::from_uri(format!(
            "http://{addr}/mcp"
        )))
        .await
        .expect("connect gateway");
    let listed = client.list_all_tools().await.expect("published tools");
    for (name, schema) in cases {
        let name = format!("capture__{name}");
        let published = listed.iter().find(|tool| tool.name == name).expect("tool");
        assert_eq!(published.input_schema["properties"]["body"], schema);
        let result = client
            .call_tool(
                CallToolRequestParams::new(name.clone())
                    .with_arguments(json!({"body":7}).as_object().expect("arguments").clone()),
            )
            .await
            .expect("tools/call");
        assert_ne!(result.is_error, Some(true), "{name}");
        let content = serde_json::to_value(&result.content).expect("content");
        let seen: Value = serde_json::from_str(content[0]["text"].as_str().expect("text"))
            .expect("capture response");
        assert_eq!(seen["wire"], "7", "{name}");
    }
    for (arguments, wire) in [(json!({"body":null}), "null"), (json!({}), "")] {
        let result = client
            .call_tool(
                CallToolRequestParams::new("capture__nullable")
                    .with_arguments(arguments.as_object().expect("arguments").clone()),
            )
            .await
            .expect("nullable call");
        assert_ne!(result.is_error, Some(true));
        let content = serde_json::to_value(&result.content).expect("content");
        let seen: Value = serde_json::from_str(content[0]["text"].as_str().expect("text"))
            .expect("capture response");
        assert_eq!(seen["wire"], wire);
    }
    client.cancel().await.expect("close client");
}

#[derive(Clone)]
struct Pages {
    character: char,
    length: usize,
    pages: usize,
}

impl ServerHandler for Pages {
    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let page = request
            .and_then(|params| params.cursor)
            .map_or(1, |cursor| cursor.parse::<usize>().expect("page cursor"));
        let schema = json!({"type":"object"})
            .as_object()
            .expect("schema")
            .clone();
        let mut result = ListToolsResult::with_all_items(vec![Tool::new(
            format!("tool_{page}"),
            self.character.to_string().repeat(self.length),
            schema,
        )]);
        if page < self.pages {
            result.next_cursor = Some((page + 1).to_string());
        }
        Ok(result)
    }

    fn get_info(&self) -> ServerConfig {
        let mut config = ServerConfig::default();
        config.capabilities = ServerCapabilities::builder().enable_tools().build();
        config
    }
}

async fn upstream(character: char, length: usize, pages: usize) -> McpUpstream {
    let pages = Pages {
        character,
        length,
        pages,
    };
    let mut config = StreamableHttpServerConfig::default();
    config.json_response = true;
    let service = StreamableHttpService::new(
        move || Ok(pages.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let addr = serve(Router::new().nest_service("/mcp", service)).await;
    McpUpstream::new(format!("http://{addr}/mcp"))
}

async fn raw_list(url: &str, cursor: Option<&str>, id: Value) -> Value {
    let mut params = json!({"_meta":{
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientInfo":{"name":"contract-test","version":"1"},
        "io.modelcontextprotocol/clientCapabilities":{}
    }});
    if let Some(cursor) = cursor {
        params["cursor"] = json!(cursor);
    }
    let response = reqwest::Client::new()
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "tools/list")
        .json(&json!({"jsonrpc":"2.0","id":id,"method":"tools/list","params":params}))
        .send()
        .await
        .expect("read upstream page");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let wire = response.bytes().await.expect("page bytes");
    assert!(
        wire.len() < 16 * 1024 * 1024,
        "page must fit the pinned SDK cap"
    );
    serde_json::from_slice(&wire).expect("JSON response")
}

async fn assert_readable_page(upstream: &McpUpstream, cursor: Option<&str>) {
    let value = raw_list(&upstream.url, cursor, json!(17)).await;
    assert_eq!(value["result"]["tools"].as_array().map(Vec::len), Some(1));
    assert!(value.get("error").is_none());
}

// 若只计工具字段而漏掉实际请求 id，工具列表本身能放下，最终 envelope 却会超预算。
#[tokio::test]
async fn list_budget_includes_the_serialized_json_rpc_envelope() {
    let upstream = upstream('x', 14 * 1024 * 1024 - 1024, 1).await;
    assert_readable_page(&upstream, None).await;
    let bridge = RmcpBridge::connect(&upstream)
        .await
        .expect("connect upstream");
    assert_eq!(
        bridge.list_tools().await.expect("tool fits budget").len(),
        1
    );
    let addr = gateway(McpGateway::new([(
        "large".into(),
        Arc::new(bridge) as Arc<dyn McpBridge>,
    )]))
    .await;
    let id = json!("\\\0".repeat(1024));
    let response = raw_list(&format!("http://{addr}/mcp"), None, id.clone()).await;
    assert_eq!(response["id"], id);
    assert_eq!(response["error"]["code"], -32603);
    assert!(response.get("result").is_none());
}

// 把预算放回 ACL 前，会错误拒绝只获准看见一个工具的调用者。
#[tokio::test]
async fn denied_inventory_does_not_use_the_callers_byte_budget() {
    let mut bridges = Vec::new();
    for name in ["allowed", "denied"] {
        let upstream = upstream('\0', 2 * 1024 * 1024, 1).await;
        let bridge = RmcpBridge::connect(&upstream)
            .await
            .expect("connect upstream");
        bridges.push((name.into(), Arc::new(bridge) as Arc<dyn McpBridge>));
    }
    let mcp = McpGateway::new(bridges).with_tool_acl(ToolAcl::from_allowed(Some(&[
        "allowed__tool_1".to_string(),
    ])));
    let counts = mcp.tools_list_counts();
    let addr = gateway(mcp).await;
    let client = ()
        .serve(StreamableHttpClientTransport::from_uri(format!(
            "http://{addr}/mcp"
        )))
        .await
        .expect("connect gateway");
    let result = client
        .list_tools(None)
        .await
        .expect("permitted inventory fits budget");
    assert_eq!(result.tools.len(), 1);
    assert_eq!(result.tools[0].name, "allowed__tool_1");
    let counts = counts.get().expect("handler records counts");
    assert_eq!((counts.total, counts.returned), (2, 1));
    client.cancel().await.expect("close client");
}

// 只改回原文长度估算，会重新读全两页并让客户端在 transport 层失败。
#[tokio::test]
async fn escaped_pages_are_bounded_and_an_oversized_upstream_is_skipped() {
    for (character, length) in [('\0', 2 * 1024 * 1024), ('\\', 5 * 1024 * 1024)] {
        let large = upstream(character, length, 2).await;
        assert_readable_page(&large, None).await;
        assert_readable_page(&large, Some("2")).await;
        let bridge = RmcpBridge::connect(&large).await.expect("connect upstream");
        let error = match bridge.list_tools().await {
            Err(error) => error,
            Ok(tools) => panic!(
                "escaped pages must exceed list budget, got {} tools",
                tools.len()
            ),
        };
        assert!(matches!(error, McpError::Request(_)));
        assert!(
            error.to_string().contains("exceeds 14680064 bytes"),
            "{error}"
        );

        let healthy = upstream('x', 8, 1).await;
        let addr = gateway(McpGateway::new([
            (
                "large".into(),
                Arc::new(EphemeralBridge::new(large)) as Arc<dyn McpBridge>,
            ),
            (
                "healthy".into(),
                Arc::new(EphemeralBridge::new(healthy)) as Arc<dyn McpBridge>,
            ),
        ]))
        .await;
        let client = ()
            .serve(StreamableHttpClientTransport::from_uri(format!(
                "http://{addr}/mcp"
            )))
            .await
            .expect("connect gateway");
        let result = client
            .list_tools(None)
            .await
            .expect("readable graceful skip result");
        assert_eq!(result.tools.len(), 1);
        assert_eq!(result.tools[0].name, "healthy__tool_1");
        client.cancel().await.expect("close client");
    }
}

// 每个 upstream 本身都在预算内；只有最终聚合结果会超限。
#[tokio::test]
async fn escaped_tools_from_separate_upstreams_return_a_json_rpc_size_error() {
    for (character, length) in [('\0', 2 * 1024 * 1024), ('\\', 5 * 1024 * 1024)] {
        let mut bridges = Vec::new();
        for name in ["alpha", "beta"] {
            let upstream = upstream(character, length, 1).await;
            assert_readable_page(&upstream, None).await;
            let bridge = RmcpBridge::connect(&upstream)
                .await
                .expect("connect upstream");
            assert_eq!(
                bridge
                    .list_tools()
                    .await
                    .expect("single page fits budget")
                    .len(),
                1
            );
            bridges.push((name.into(), Arc::new(bridge) as Arc<dyn McpBridge>));
        }
        let addr = gateway(McpGateway::new(bridges)).await;
        let client = ()
            .serve(StreamableHttpClientTransport::from_uri(format!(
                "http://{addr}/mcp"
            )))
            .await
            .expect("connect gateway");
        let error = client
            .list_tools(None)
            .await
            .expect_err("aggregate exceeds budget");
        let ServiceError::McpError(error) = error else {
            panic!("size failure must be readable JSON-RPC, got {error}");
        };
        assert_eq!(error.code.0, -32603);
        assert!(
            error
                .message
                .starts_with("aggregated tools/list is too large"),
            "{error}"
        );
        client.cancel().await.expect("close client");
    }
}

// 把 total 移到 retain 后会得到 0/0，此断言必须能检出。
#[tokio::test]
async fn denied_tools_are_still_counted_before_acl_filtering() {
    let upstream = upstream('x', 8, 2).await;
    let bridge = RmcpBridge::connect(&upstream)
        .await
        .expect("connect upstream");
    let mcp = McpGateway::new([("counted".into(), Arc::new(bridge) as Arc<dyn McpBridge>)])
        .with_tool_acl(ToolAcl::from_allowed(Some(&[])));
    let counts = mcp.tools_list_counts();
    let addr = gateway(mcp).await;
    let client = ()
        .serve(StreamableHttpClientTransport::from_uri(format!(
            "http://{addr}/mcp"
        )))
        .await
        .expect("connect gateway");
    assert!(client
        .list_tools(None)
        .await
        .expect("empty list")
        .tools
        .is_empty());
    let counts = counts.get().expect("handler must record counts");
    assert_eq!((counts.total, counts.returned), (2, 0));
    client.cancel().await.expect("close client");
}
