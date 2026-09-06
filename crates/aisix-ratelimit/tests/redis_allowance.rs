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

/// 每个用例一个键空间。共用一台 Redis 时靠它互不干扰（见 `Redis::start`）。
static NS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// 一台可用的 Redis：要么是别人给的（`RATELIMIT_TEST_REDIS_URL`），要么是这里
/// 起的用完即弃的实例。
struct Redis {
    /// 自己起的那种才有；用别人给的时候是 `None`。
    owned: Option<(Child, std::path::PathBuf)>,
    url: String,
    /// 这个用例独占的键前缀命名空间。
    ns: String,
}

impl Redis {
    /// `None` **只表示这台机器上既没有给定的 Redis、也没有 redis-server**。
    ///
    /// 优先用 `RATELIMIT_TEST_REDIS_URL`：CI 上的 Redis 是容器服务，runner 的 PATH
    /// 上没有 `redis-server` 二进制 —— 只会自己起的话，这些用例在 CI 里是**静默
    /// 跳过**的，而它们守的是累计额度与「批处理不重复收费」那道闸。共用一台时
    /// 每个用例用自己的键前缀，互不影响。
    ///
    /// 变量名沿用这个 crate 里 `redis_integration.rs` 已有的那一个（CI 早就设
    /// 好了），而不是另起一个。另起的名字**绝不能以 `AISIX_` 开头**：
    /// `Config::load_from_path` 会把所有 `AISIX_*` 合并进根配置，而 `Config` 带
    /// `deny_unknown_fields` —— 那会让 `aisix-core::config` 的整组测试炸掉。
    /// CI 里那段注释写的就是这件事，我第一版起的名正好踩中。
    ///
    /// 探测通过之后的任何失败都直接 panic：那是环境问题，混进 `None` 会让整套
    /// 测试静默跳过 —— 一套永远绿的测试比没有测试更糟，因为它还让人放心。
    fn start() -> Option<Self> {
        // 掺进 pid：计数器每个进程从 0 开始，而共用的那台 Redis 是跨进程存在的
        // —— 只用计数器的话，连跑两遍时第二遍会读到第一遍留下的键，症状是「计
        // 数器的键不是恰好一个」，看起来像被测代码坏了。（这条是被这套测试自己
        // 抓出来的。）
        //
        // 共用实例上这些键会留下来。CI 的 Redis 是每个 job 一份的容器，随 job
        // 消失；指向长期实例时它会慢慢攒 —— 那是这个 env 变量的已知代价。
        let ns = format!(
            "t{}-{}",
            std::process::id(),
            NS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        if let Ok(url) = std::env::var("RATELIMIT_TEST_REDIS_URL") {
            if !url.trim().is_empty() {
                return Some(Self {
                    owned: None,
                    url: url.trim().to_string(),
                    ns,
                });
            }
        }
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
        Some(Self {
            owned: Some((child, dir)),
            url: format!("redis://127.0.0.1:{port}"),
            ns,
        })
    }

    fn url(&self) -> String {
        self.url.clone()
    }

    /// 把自己起的那台 Redis 打掉，模拟运行中的中断。
    ///
    /// 只对自己起的实例有效 —— 共用那台不能动，所以用到它的用例在
    /// `RATELIMIT_TEST_REDIS_URL` 下会跳过（并说明为什么）。
    fn kill_owned(&mut self) -> bool {
        match self.owned.as_mut() {
            Some((child, _)) => {
                let _ = child.kill();
                let _ = child.wait();
                true
            }
            None => false,
        }
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

    /// 按**本用例的命名空间**扫键。共用一台 Redis 时，裸 `KEYS *consumed*` 会
    /// 扫到别的用例的键 —— 而那种污染的症状是「键不是恰好一个」，看起来像被测
    /// 代码的问题。
    async fn keys(&self, suffix_glob: &str) -> Vec<String> {
        let mut c = self.raw().await;
        redis::cmd("KEYS")
            .arg(format!("*:{}:*{suffix_glob}", self.ns))
            .query_async(&mut c)
            .await
            .expect("KEYS 可用")
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
        // 别人给的那台不归这里收 —— 只清自己起的。
        if let Some((child, dir)) = self.owned.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(dir);
        }
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
        // 共用一台 Redis 时，键前缀是用例之间唯一的隔离手段。自己起实例时它
        // 无害，所以两种情形走同一条路 —— 分叉的话，只有一边会被跑到。
        .with_env_namespace(&r.ns)
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
                    // CI 上跳过 = 静默无覆盖。这几条守的是累计额度与「批处理
                    // 不重复收费」那道闸，而 CI 的 Redis 是容器服务、PATH 上没
                    // 有 `redis-server` —— 只靠自起的话它们从来没跑过，而没人
                    // 会注意到一行 stderr。给 `RATELIMIT_TEST_REDIS_URL` 指一台就行。
                    assert!(
                        std::env::var("CI").is_err(),
                        "CI 上没有可用的 Redis：既没有 RATELIMIT_TEST_REDIS_URL，\
                         PATH 上也没有 redis-server。跳过就是静默无覆盖。"
                    );
                    eprintln!("跳过：本机既没有 RATELIMIT_TEST_REDIS_URL 也没有 redis-server");
                    return;
                }
                Some(r) => {
                    if r.wait_ready().await {
                        got = Some(r);
                        break;
                    }
                    eprintln!("第 {attempt} 次连 redis 没就绪（{}），重来", r.url());
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
    let keys = r.keys("consumed*").await;
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
        let keys = r.keys("consumed*").await;
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
    let keys = r.keys("claim:batch-spend:b1*").await;
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

/// Redis 挂掉时限流器**放行**（fail-open），并且这件事被计量出来。
///
/// 放行本身是有意的：共享计数器不可达时把所有流量拒掉，等于让一次 Redis 故障
/// 变成一次全站宕机。代价是故障期间集群级限额不再生效 —— 每个副本各算各的。
/// 所以这不是一个可以静悄悄发生的降级，运维必须能看见它，而唯一的看见方式就是
/// `record_redis_failure` 这个计数。
///
/// 之前没有任何测试断言它被调用过：这个 crate 里唯一实现了这个方法的测试 sink
/// 是空实现。少这一笔的症状是「一条永远不出现的曲线」，和「Redis 从没坏过」
/// 长得一模一样。
#[tokio::test]
async fn a_dead_redis_falls_open_and_says_so() {
    let Some(mut redis) = Redis::start() else {
        eprintln!("跳过：既没有 RATELIMIT_TEST_REDIS_URL，也没有 redis-server");
        return;
    };
    if !redis.wait_ready().await {
        panic!("redis 起来了却 PING 不通 —— 环境问题，不静默跳过");
    }

    #[derive(Default)]
    struct FailureSink {
        ops: std::sync::Mutex<Vec<String>>,
    }
    impl aisix_core::RateLimitMetricsSink for FailureSink {
        fn record_redis_failure(&self, op: &str) {
            self.ops.lock().expect("sink").push(op.to_string());
        }
        fn record_post_stream_shed(&self, _dropped: u64) {}
    }

    let sink = std::sync::Arc::new(FailureSink::default());
    let store = store(&redis).await.with_metrics(sink.clone());

    let limits = RateLimit {
        rpm: Some(1_000),
        ..RateLimit::default()
    };
    let key = "fail-open-key";

    // 先证明连着的时候是真的走 Redis：这一步不成立的话，后面「挂掉之后还放行」
    // 什么都证明不了。
    store
        .acquire(key, &limits, "m-1")
        .await
        .expect("healthy redis admits the first request");
    assert!(
        sink.ops.lock().expect("sink").is_empty(),
        "a healthy redis must not be counted as a failure"
    );

    if !redis.kill_owned() {
        eprintln!("跳过后半段：共用的 Redis 不能打掉（RATELIMIT_TEST_REDIS_URL）");
        return;
    }

    // 断开之后：这一次调用必须**及时**返回并放行，同时把降级记上一笔。
    //
    // 「及时」是断言的一部分，不是背景。第一版这里写的是「最多重试 20 次直到
    // 计数出现」，结果整个用例挂死了十几分钟 —— 因为 redis-rs 默认既没有命令
    // 超时也没有连接超时，每一次失败的 acquire 都在重连阶梯上排队。挂死本身
    // 就是要证的东西：一个不封顶的等待不是 fail-open，是 hang。上界现在由
    // `aisix-redis` 里那三个常量兜住（见 `blackhole_timeout` 用例）。
    let started = std::time::Instant::now();
    store
        .acquire(key, &limits, "m-after")
        .await
        .expect("a dead redis must FALL OPEN, not refuse the request");
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "falling open took {elapsed:?} — the caller waits through all of it \
         before the local counter takes over"
    );
    let ops = sink.ops.lock().expect("sink").clone();
    assert!(
        !ops.is_empty(),
        "the store fell open without counting it — an operator cannot tell \
         a degraded cluster from a quiet one; got {ops:?}"
    );
    assert!(
        ops.iter().any(|o| o == "acquire"),
        "the failure must name the operation that degraded; got {ops:?}"
    );
}
