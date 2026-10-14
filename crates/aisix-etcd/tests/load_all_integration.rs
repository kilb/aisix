//! Integration test: a full-prefix load is paged, so a configuration larger
//! than one request's budget still loads.
//!
//! Requires a real etcd — gated by `ETCD_TEST_URL`. CI sets this via the
//! etcd service container; local runs without the var are no-ops.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aisix_etcd::provider::ConfigProvider;
use aisix_etcd::{ConnectPolicy, EtcdConfigProvider};
use etcd_client::{Client, DeleteOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn etcd_url() -> Option<String> {
    std::env::var("ETCD_TEST_URL").ok()
}

fn unique_prefix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("/aisix-etcd-load-it/{nanos:x}-{}", std::process::id())
}

/// 限速转发器：etcd 往回发的字节按 `bytes_per_sec` 吐给客户端，模拟一条慢
/// 链路；客户端发出的方向不限速。
async fn throttled_proxy(etcd_url: &str, bytes_per_sec: usize) -> String {
    let target = etcd_url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .expect("ETCD_TEST_URL carries host:port")
        .to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let url = format!("http://{}", listener.local_addr().expect("proxy addr"));
    tokio::spawn(async move {
        while let Ok((inbound, _)) = listener.accept().await {
            let target = target.clone();
            tokio::spawn(async move {
                let Ok(outbound) = tokio::net::TcpStream::connect(&target).await else {
                    return;
                };
                let (mut in_read, mut in_write) = inbound.into_split();
                let (mut out_read, mut out_write) = outbound.into_split();
                tokio::spawn(async move {
                    let _ = tokio::io::copy(&mut in_read, &mut out_write).await;
                });
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    let n = match out_read.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    if in_write.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                    let pause = n as f64 / bytes_per_sec as f64;
                    tokio::time::sleep(Duration::from_secs_f64(pause)).await;
                }
            });
        }
    });
    url
}

/// 全量加载是一批数据，不是一个请求。`request_timeout` 约束的是每一次范围
/// 读；一份大于一次请求预算的配置，在慢链路上也必须能加载完 —— 否则启动
/// 直接失败，watch 重置后的重新同步每次都超时，网关一直停在旧配置上。
///
/// 3000 个约 1KB 的值、1MB/s 的链路、1s 的请求预算：一次读完要三秒左右，
/// 分页读每页远在预算之内。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prefix_larger_than_one_request_budget_still_loads() {
    let Some(url) = etcd_url() else {
        eprintln!("ETCD_TEST_URL not set — skipping");
        return;
    };
    const KEYS: usize = 3000;
    let prefix = unique_prefix();
    let mut seed = Client::connect([url.as_str()], None)
        .await
        .expect("connect etcd");
    let value = "x".repeat(1024);
    for i in 0..KEYS {
        seed.put(format!("{prefix}/models/{i:05}"), value.as_str(), None)
            .await
            .expect("seed key");
    }

    let proxy = throttled_proxy(&url, 1_000_000).await;
    let provider = EtcdConfigProvider::connect_with_policy(
        &[proxy],
        prefix.clone(),
        None,
        ConnectPolicy {
            request_timeout: Some(Duration::from_secs(1)),
            ..ConnectPolicy::default()
        },
    )
    .await
    .expect("connect through the proxy");

    let loaded = provider.load_all().await;

    seed.delete(prefix.as_str(), Some(DeleteOptions::new().with_prefix()))
        .await
        .expect("clean up");
    let (entries, revision) = loaded.expect("a large prefix must load page by page");
    assert_eq!(entries.len(), KEYS, "every key under the prefix is loaded");
    assert!(revision > 0);
}
