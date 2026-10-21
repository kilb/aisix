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

/// 分页读出来的必须是**同一个修订版本**上的一份快照：第一页之后发生的写入
/// 不能混进后面的页里 —— 否则快照里有的键属于新修订、有的属于旧修订，watch
/// 又从第一页的修订接着走，删掉的键会凭空留下或者新键被重复应用。
///
/// 和上一条一样走限速链路，让加载持续约三秒；在第一页之后删掉最后一页里的
/// 一个键、新建一个键。快照里必须还有被删的那个、没有新建的那个，返回的
/// 修订版本早于这两次写入。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_page_reads_the_first_pages_revision() {
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

    let load = tokio::spawn(async move { provider.load_all().await });
    // 第一页（约 0.5MB，1MB/s）读完之后、最后一页读到之前。
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(!load.is_finished(), "the load must still be paging");
    let deleted = format!("{prefix}/models/{:05}", KEYS - 1);
    let added = format!("{prefix}/models/{KEYS:05}");
    let del = seed.delete(deleted.as_str(), None).await.expect("delete");
    seed.put(added.as_str(), "y", None).await.expect("put");
    let write_revision = del.header().map(|h| h.revision()).unwrap_or(i64::MAX);

    let loaded = load.await.expect("join");
    seed.delete(prefix.as_str(), Some(DeleteOptions::new().with_prefix()))
        .await
        .expect("clean up");
    let (entries, revision) = loaded.expect("load");
    let keys: std::collections::HashSet<&str> = entries.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(
        entries.len(),
        KEYS,
        "one revision: neither the delete nor the put"
    );
    assert!(
        keys.iter()
            .any(|k| k.ends_with(&format!("/models/{:05}", KEYS - 1))),
        "a key deleted after the first page is still in the snapshot"
    );
    assert!(
        !keys
            .iter()
            .any(|k| k.ends_with(&format!("/models/{KEYS:05}"))),
        "a key created after the first page must not leak into a later page"
    );
    assert!(revision < write_revision, "{revision} vs {write_revision}");
}
