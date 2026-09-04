//! 累计额度在**真 Redis** 上的行为。
//!
//! 为什么要单独一套：`granted_micro_usd` 这条闸在 Redis 后端里是一段 Lua，而那
//! 段 Lua 从没在真服务器上执行过 —— 单测只比对过两个脚本的文本，能证明它们touch
//! 同一批键，证明不了 Redis 会怎么执行它们。而这是多副本生产会走的那个后端：
//! 本地 store 的计数器每个副本一份，只有 Redis 这条能给出全局的累计额。
//!
//! 自带一个实例（随机端口、独立目录、跑完就收），所以不依赖机器上恰好有没有
//! Redis 在跑，也不会污染别人的库。没有 `redis-server` 时整套跳过并说明原因 ——
//! 静默通过等于没测。

use std::process::{Child, Command};

use aisix_core::config::{RedisConnConfig, RedisMode};
use aisix_core::RateLimit;
use aisix_ratelimit::{RateLimitError, RateStore};

/// 一个用完即弃的 redis-server。
struct Redis {
    child: Child,
    port: u16,
    dir: std::path::PathBuf,
}

impl Redis {
    /// `None` **只表示机器上没有 redis-server**。
    ///
    /// 探测通过之后的任何失败都直接 panic：那是环境问题，混进 `None` 会让整套
    /// 测试静默跳过 —— 一套永远绿的测试比没有测试更糟，因为它还让人放心。
    fn start() -> Option<Self> {
        if Command::new("redis-server")
            .arg("--version")
            .output()
            .is_err()
        {
            return None;
        }
        // 端口：先绑一个 0 让内核挑，拿到号再放开给 redis 用。窗口极小，而
        // 写死端口会在并行跑的时候撞车。
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("挑一个空闲端口");
            l.local_addr().expect("读回本地地址").port()
        };
        let dir = std::env::temp_dir().join(format!("aisix-redis-test-{port}"));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let child = Command::new("redis-server")
            .args([
                "--port",
                &port.to_string(),
                "--bind",
                "127.0.0.1",
                "--save",
                "",
                "--appendonly",
                "no",
                "--dir",
                &dir.to_string_lossy(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        // 目录在构造守卫**之前**就建好了（redis 启动就要用它），所以这里失败必须
        // 自己收拾 —— 否则 /tmp 里会攒下一堆空目录，而 `Drop` 压根没机会跑。
        let child = match child {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                panic!("redis-server 探测通过却起不起来（{e}）—— 环境问题，不当作「没装」跳过");
            }
        };
        Some(Self { child, port, dir })
    }

    fn url(&self) -> String {
        format!("redis://127.0.0.1:{}", self.port)
    }

    async fn wait_ready(&self) -> bool {
        for _ in 0..100 {
            if let Ok(client) = redis::Client::open(self.url()) {
                if let Ok(mut c) = client.get_multiplexed_async_connection().await {
                    if redis::cmd("PING")
                        .query_async::<String>(&mut c)
                        .await
                        .is_ok()
                    {
                        return true;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        false
    }

    /// 直接读一个键，用来断言脚本真的写了它（以及没给它设过期）。
    async fn raw(&self) -> redis::aio::MultiplexedConnection {
        redis::Client::open(self.url())
            .expect("client")
            .get_multiplexed_async_connection()
            .await
            .expect("conn")
    }
}

impl Drop for Redis {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn granted(n: u64) -> RateLimit {
    RateLimit {
        granted_micro_usd: Some(n),
        ..RateLimit::default()
    }
}

async fn store(r: &Redis) -> aisix_ratelimit::store::redis::RedisStore {
    let cfg = RedisConnConfig {
        mode: RedisMode::Single,
        url: Some(r.url()),
        ..RedisConnConfig::default()
    };
    aisix_ratelimit::store::redis::RedisStore::connect(&cfg)
        .await
        .expect("connect")
}

/// 只在**机器上没有 redis-server** 时跳过，并说明原因。
///
/// 「装着但起不来」不跳过，直接失败：那是环境出了问题，跳过它就成了静默通过 ——
/// 一套永远绿的测试比没有测试更糟，因为它还让人放心。
macro_rules! redis_or_skip {
    () => {{
        // 端口是「先 bind(0) 拿号、关掉、再让 redis 去 bind」拿到的，中间有一个
        // 极小的窗口 —— 并行跑的另一条用例可能刚好抢到同一个号，于是 redis 起不
        // 来。那不是环境问题，重来一次就好；而把它算成环境问题会让这套用例偶尔
        // 无理由地红。
        //
        // 重试有上界：真的环境问题不该被无限重试掩盖成一次超时。
        let mut got = None;
        for attempt in 1..=3 {
            match Redis::start() {
                None => {
                    eprintln!("跳过：机器上没有 redis-server");
                    return;
                }
                Some(r) => {
                    if r.wait_ready().await {
                        got = Some(r);
                        break;
                    }
                    eprintln!("第 {attempt} 次起 redis 没就绪（端口 {}），重来", r.port);
                    // r 在这里析构，进程与目录都收掉。
                }
            }
        }
        got.expect("redis-server 存在却连着三次起不来 —— 这是环境问题，不跳过")
    }};
}

#[tokio::test]
async fn 累计额度用尽后拒绝_且跨窗口不重置() {
    let r = redis_or_skip!();
    let s = store(&r).await;
    let lim = granted(1_000);

    s.acquire("k", &lim, "").await.expect("首次放行");
    s.commit("k", 600, "").await;
    s.acquire("k", &lim, "").await.expect("600 < 1000，仍放行");
    s.commit("k", 600, "").await;

    // 已消费 1200 ≥ 1000。
    assert!(
        matches!(
            s.acquire("k", &lim, "").await,
            Err(RateLimitError::AllowanceExhausted)
        ),
        "累计额度用尽却还放行"
    );

    // **累计计数器不能有过期时间。** 有的话它会在某个时刻悄悄归零，用户凭空
    // 多出一份额度 —— 而这件事不会有任何报错。
    //
    // 键名扫出来而不是写死：布局带 Cluster 哈希标签（`aisix:rl:{k}:consumed`），
    // 写死一次猜错就会变成一条「键不存在、TTL=-2」的假通过。
    let mut c = r.raw().await;
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg("*consumed*")
        .query_async(&mut c)
        .await
        .expect("KEYS");
    assert_eq!(keys.len(), 1, "累计计数器的键不是恰好一个: {keys:?}");
    let key = &keys[0];
    let ttl: i64 = redis::cmd("TTL")
        .arg(key)
        .query_async(&mut c)
        .await
        .expect("TTL");
    assert_eq!(ttl, -1, "{key} 被设了过期时间（TTL={ttl}）");
    let v: i64 = redis::cmd("GET")
        .arg(key)
        .query_async(&mut c)
        .await
        .expect("GET");
    assert_eq!(v, 1_200, "累计值不对");
}

#[tokio::test]
async fn 流式那条记账路径也计入累计额() {
    let r = redis_or_skip!();
    let s = store(&r).await;
    let lim = granted(1_000);

    // 流式走的是 add_tokens（流结束时的同步回调），不是 commit。两条记的是同一
    // 份账，漏掉任何一条，那道闸对该模式的流量就等于不存在 —— 本地 store 上曾经
    // 就漏了这一条。
    s.acquire("k", &lim, "").await.expect("首次放行");
    s.add_tokens("k", 1_200);
    // add_tokens 在 Redis 后端是异步落到后台 worker 的，等它写进去。
    let mut c = r.raw().await;
    for _ in 0..100 {
        let keys: Vec<String> = redis::cmd("KEYS")
            .arg("*consumed*")
            .query_async(&mut c)
            .await
            .expect("KEYS");
        let v: i64 = match keys.first() {
            Some(k) => redis::cmd("GET")
                .arg(k)
                .query_async(&mut c)
                .await
                .unwrap_or(0),
            None => 0,
        };
        if v >= 1_200 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    assert!(
        matches!(
            s.acquire("k", &lim, "").await,
            Err(RateLimitError::AllowanceExhausted)
        ),
        "流式记的账没有计入累计额"
    );
}

#[tokio::test]
async fn 额度调高之后立刻放行_不需要重置计数器() {
    let r = redis_or_skip!();
    let s = store(&r).await;

    s.acquire("k", &granted(1_000), "").await.expect("放行");
    s.commit("k", 1_500, "").await;
    assert!(s.acquire("k", &granted(1_000), "").await.is_err());

    // 推下去一个更大的数就该放行 —— 这正是「充值后立刻恢复」依赖的性质：
    // 累计值只增不减，闸靠比较，不需要任何对账或重置。
    s.acquire("k", &granted(5_000), "")
        .await
        .expect("额度调高之后仍被拒");
}

/// `claim_once` 是给「会重放、但记账不能重复」的副作用用的：批处理跑完之后的
/// 花费归因就是这种 —— 它的用量事件带确定性 id，消费端能去重，而只增的花费
/// 计数器和 Prometheus 计数器都没有那层保护，重放一次就是多收一遍。
///
/// 用两个各自 `connect` 出来的存储，模拟「重启后的同一个网关」与「另一个实例」
/// —— 它们不共享任何进程内状态，只共享 Redis。标记要在这种情况下仍然只发一次。
#[tokio::test]
async fn 一次性标记跨实例只发一次() {
    let r = redis_or_skip!();
    let a = store(&r).await;
    let b = store(&r).await;
    let ttl = std::time::Duration::from_secs(600);

    assert!(
        a.claim_once("batch-spend:b1", ttl).await,
        "第一次该拿到标记"
    );
    assert!(
        !a.claim_once("batch-spend:b1", ttl).await,
        "同一个存储重来一次不该再拿到"
    );
    assert!(
        !b.claim_once("batch-spend:b1", ttl).await,
        "另一个实例（也等价于重启后的自己）不该再拿到 —— 这正是本地内存那份\
         做不到、必须放进共享存储的原因"
    );
    // 另一个批次互不影响。
    assert!(
        b.claim_once("batch-spend:b2", ttl).await,
        "不同批次的标记被互相挡住了"
    );

    // TTL 真的落在键上：没有过期时间的标记会永久占着 Redis。
    let mut conn = r.raw().await;
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg("*claim:batch-spend:b1*")
        .query_async(&mut conn)
        .await
        .expect("KEYS 可用");
    assert_eq!(keys.len(), 1, "标记键没建出来: {keys:?}");
    let secs: i64 = redis::cmd("TTL")
        .arg(&keys[0])
        .query_async(&mut conn)
        .await
        .expect("TTL 可用");
    assert!(
        secs > 0 && secs <= 600,
        "标记键的 TTL 不对（{secs}）—— -1 表示永不过期，那是泄漏"
    );
}
