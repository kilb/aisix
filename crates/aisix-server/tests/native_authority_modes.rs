//! 真实进程启动和受保护 SQLite 验证单机归属；本地 HTTP 上游仅模拟公开协议。
#![cfg(unix)]

use std::net::TcpListener;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aisix_core::ApiKey;
use reqwest::StatusCode;
use serde_json::{json, Value};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Gateway {
    child: Option<Child>,
    directory: tempfile::TempDir,
    config: PathBuf,
    base: String,
    client: reqwest::Client,
    upstream: MockServer,
}

impl Gateway {
    async fn new(authority: bool) -> Self {
        let upstream = MockServer::builder().start().await;
        let sequence = AtomicUsize::new(1);
        Mock::given(method("POST"))
            .and(path_regex("^/v1/responses(/input_tokens)?$"))
            .respond_with(move |request: &wiremock::Request| {
                if request.url.path().ends_with("/input_tokens") {
                    return ResponseTemplate::new(200)
                        .set_body_json(json!({"object":"response.input_tokens","input_tokens":5}));
                }
                let index = sequence.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(json!({
                    "id":format!("resp_mode_{index}"),"object":"response",
                    "status":"completed","model":"upstream-model",
                    "output":[{"type":"reasoning","id":format!("rs_mode_{index}"),
                        "summary":[{"type":"summary_text","text":"visible prior reasoning"}]}],
                    "usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}
                }))
            })
            .mount(&upstream)
            .await;
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let resources = directory.path().join("resources.json");
        std::fs::write(
            &resources,
            serde_json::to_vec(&json!({
                "_format_version":"1",
                "provider_keys":[{"display_name":"native-account","provider":"openai",
                    "adapter":"openai","api_base":upstream.uri(),"api_key":"local-upstream-key"}],
                "models":[{"display_name":"native-model","provider":"openai",
                    "model_name":"upstream-model","provider_key":"native-account"}],
                "api_keys":[
                    {"display_name":"owner","key_hash":ApiKey::hash_bearer("mode-owner"),
                        "allowed_models":["native-model"],"allowed_endpoints":["responses"]},
                    {"display_name":"other-owner","key_hash":ApiKey::hash_bearer("mode-other"),
                        "allowed_models":["native-model"],"allowed_endpoints":["responses"]}
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let mut config = json!({
            "resources_file":resources,"managed":{"enabled":false},
            "proxy":{"addr":address.to_string(),"workers":1,"platform":{"enabled":false}},
            "admin":{"enabled":false},
            "observability":{"metrics":{"prometheus":{"enabled":false}},
                "debug":{"enabled":false},"heap_profiling":{"auto_dump":{"enabled":false}}},
            "upstream":{"retries":0}
        });
        if authority {
            let database = directory.path().join("authority.sqlite3");
            drop(
                std::fs::OpenOptions::new()
                    .create_new(true)
                    .read(true)
                    .write(true)
                    .mode(0o600)
                    .open(&database)
                    .unwrap(),
            );
            let secret = directory.path().join("origin-secret");
            std::fs::write(&secret, "a3".repeat(32)).unwrap();
            std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
            config["native_objects"] = json!({"sqlite_path":database,"origin_secret_file":secret});
        }
        let config_path = directory.path().join("config.json");
        std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        let mut gateway = Self {
            child: None,
            directory,
            config: config_path,
            base: format!("http://{address}"),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            upstream,
        };
        gateway.start().await;
        gateway
    }

    async fn start(&mut self) {
        let log = std::fs::File::create(self.directory.path().join("gateway.log")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_aisix"));
        command
            .arg("--config")
            .arg(&self.config)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log));
        // 子进程只读取本测试配置，不继承其他会话的启动覆盖。
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("AISIX_") {
                command.env_remove(name);
            }
        }
        self.child = Some(command.spawn().unwrap());
        let until = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                panic!("gateway exited {status}: {}", self.log());
            }
            if self
                .client
                .get(format!("{}/livez", self.base))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                return;
            }
            assert!(tokio::time::Instant::now() < until, "{}", self.log());
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.directory.path().join("gateway.log")).unwrap()
    }

    async fn post(&self, path: &str, bearer: &str, body: Value) -> (StatusCode, Value) {
        let response = self
            .client
            .post(format!("{}{path}", self.base))
            .bearer_auth(bearer)
            .json(&body)
            .send()
            .await
            .unwrap();
        (response.status(), response.json().await.unwrap())
    }

    async fn sent(&self) -> Vec<Value> {
        self.upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.stop();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_sqlite_creates_and_continues_owned_responses_after_restart() {
    let mut gateway = Gateway::new(true).await;
    let (status, response) = gateway
        .post(
            "/v1/responses",
            "mode-owner",
            json!({"model":"native-model","input":"hello"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{response}\n{}", gateway.log());
    let previous = response["id"].as_str().unwrap().to_owned();
    let request =
        json!({"model":"native-model","input":"continue","previous_response_id":previous});
    let (status, body) = gateway
        .post("/v1/responses", "mode-other", request.clone())
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["type"], "response_not_found");
    assert_eq!(gateway.sent().await.len(), 1);
    let (status, body) = gateway
        .post(
            "/v1/responses",
            "mode-owner",
            json!({"model":"native-model","input":[response["output"][0].clone()]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = gateway
        .post("/v1/responses", "mode-owner", request.clone())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    gateway.stop();
    gateway.start().await;
    let (status, body) = gateway.post("/v1/responses", "mode-owner", request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let sent = gateway.sent().await;
    assert_eq!(sent.len(), 4);
    assert_eq!(sent[1]["input"][0]["id"], response["output"][0]["id"]);
    assert_eq!(sent[3]["previous_response_id"], previous);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_sqlite_checks_native_reasoning_even_with_visible_text() {
    let gateway = Gateway::new(true).await;
    let (status, body) = gateway
        .post(
            "/v1/responses/input_tokens",
            "mode-owner",
            json!({"model":"native-model","input":[{"type":"reasoning","id":"rs_unowned",
                "summary":[{"type":"summary_text","text":"visible prior reasoning"}]}]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(
        body["error"]["message"],
        "request field `input[0].id` names an object stored upstream, which platform mode does not allow"
    );
    assert!(gateway.sent().await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_platform_off_requests_keep_upstream_context_without_authority() {
    let gateway = Gateway::new(false).await;
    let (status, body) = gateway
        .post(
            "/v1/responses",
            "mode-owner",
            json!({"model":"native-model","input":"continue","previous_response_id":"resp_external"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let sent = gateway.sent().await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["previous_response_id"], "resp_external");
}
