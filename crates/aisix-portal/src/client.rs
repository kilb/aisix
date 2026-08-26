//! 门户的出站 HTTP 客户端。
//!
//! 不用 `aisix_gateway::client_builder()`：那个函数读的是网关配置里的全局
//! `UpstreamHttpConfig`，而门户是独立进程、从不加载网关配置 —— 与
//! `crates/aisix-console` 同一处境，也用同一套做法：**把那几个值显式钉住**。
//!
//! 光有总超时不够。reqwest 默认没有连接超时、关掉 TCP keepalive、连接池里的
//! 空闲连接留 90 秒。负载均衡器回收连接之后，池里那条已经死掉的连接还会被拿
//! 去发下一个请求 —— 表现是偶发失败一次、重试又好。
//!
//! `aisix-gateway` 里 `no_production_code_builds_a_bare_reqwest_client` 会扫
//! 整个工作区，门户在那份名单里是显式豁免的；豁免的配套条件是
//! `portal_client_is_not_left_on_reqwest_defaults` 盯着下面这几个设置。

use std::time::Duration;

/// 门户唯一的出站客户端构造点。
pub fn outbound() -> reqwest::Client {
    reqwest::Client::builder()
        // 不跟重定向。门户只往两个地方发请求：Prometheus 与（二期的）Admin
        // API，都是运维配的内网地址。真实的它们不会对查询做重定向，而跟一个
        // 302 就等于让配置里的地址决定权落到应答方手上。
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(5))
        .tcp_keepalive(Duration::from_secs(60))
        .pool_idle_timeout(Duration::from_secs(30))
        .build()
        .expect("http client")
}
