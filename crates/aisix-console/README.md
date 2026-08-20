# aisix-console

开源 AISIX 网关的单机管理界面。**不是 AISIX Cloud 的 Dashboard** —— 那是控制
平面的界面，服务多租户与集中管理；这里只管一台网关，靠重写它的声明式配置文件
工作。

它和网关同仓，因为两者之间的契约有四类且每一类都会静默漂移：调用 `aisix
validate` 二进制、resources schema 的字段名、`aisix_*` 指标名、Admin API 的形状。
分仓时踩过一次——控制台先上线了 `max_spend_micro_usd` 表单，而网关二进制还在
拒绝这个字段。同仓之后，「新配置项要连界面一起交付」这条规矩才可能在一个改动
里满足。

## 它能做什么，为什么只能做这些

网关的**管理 API 是全模式只读的** —— 写端点（POST/PUT/DELETE）被上游有意
移除了，改走声明式配置。所以本控制台的分工是：

| 能力 | 数据来源 | 备注 |
|---|---|---|
| 看资源（模型/供应商/密钥/护栏/缓存策略） | 网关管理 API `GET`（回环 :8081） | 管理密钥只在服务端 |
| 看用量（请求、token、花费） | Prometheus（回环 :9090） | 指标带 `api_key_id` / `model` / `provider` 标签，真实数据 |
| 看定价 | 模型的 `cost.{input,output}_per_1k` | 就是网关折算花费用的那份数字 |
| 改配置 | 重写 `resources.yaml` 后发 `SIGHUP` | 落盘前先跑 `aisix validate` |

按金额的预算**现在能配**了。网关把花费上限做成了本地机制
（`RateLimitPolicy.max_spend_micro_usd`），不再依赖控制平面下发，所以「限流与
预算」页可以直接设置它。注意它是软上限：约束的是已记录的花费，上限被越过那一
刻在途的请求会跑完并计账，超出量最多是那一刻并发在途请求的花费总和。

**仍然做不到的**：给已建好的模型改定价。模型只能新增和删除，改价要删了重建
——或者用「配置原文」页直接改 YAML。

## 两个不显然的实现点

**密钥名要靠 `key_hash` 对接。** 指标里只有 `api_key_id`；管理 API 有 id 和
`key_hash`，但**没有名字** —— 文件模式下 `api_keys[].display_name` 是「文件侧
身份」，校验前就被剥离（见上游 `filesource/desugar.rs`），只存在于
`resources.yaml`。`key_hash` 是两边都有的字段，用它把两侧接起来。

**花费要按量级调精度。** LLM 单次调用常在千分之一美元量级，固定两位小数会把
真实花费显示成 `$0.00`，那和「没有花费」在界面上无法区分。

## 写入路径

`版本比对 → 校验 → 原子替换 → SIGHUP`，四步全成功才算保存：

0. 比对版本。`GET /api/file` 会返回一个 `version`（文件内容的 sha256），
   保存时带回来，服务端**在写锁内重读磁盘**再比对，不一致就返回 409 且不落盘。
   没有这一步的话，`save_lock` 只能保证两次写不交错，拦不住丢失更新：两个标
   签页各持有一份整文档，后保存的会静默盖掉先保存的改动——如果被盖掉的是一次
   密钥吊销，那把密钥就又活了。
1. 序列化成临时文件；
2. 跑网关自带的 `aisix validate --resources`（不自己重实现 schema —— 重实现
   一定会漂移，而漂移方向永远是「控制台放行了网关拒绝的东西」）；
3. 校验通过才 `rename` 覆盖，坏配置绝不落盘；
4. `pkill -HUP -x aisix` 热加载。控制台以 `aisix` 用户运行，所以能直接给同
   用户的网关进程发信号，不需要任何 sudo 授权。

控制台一旦接管 `resources.yaml`，就由它整体重写 —— 手写的注释会在第一次保存
后消失。原始带注释的版本留在 `/etc/aisix/resources.yaml.orig`。

## 部署形态

```
/usr/local/bin/aisix-console          二进制
/etc/aisix/console.env                口令散列 + 管理密钥（root:aisix 640）
/var/lib/aisix-console/resources.yaml 网关的声明式配置，控制台拥有该目录
systemd: aisix-console.service        以 aisix 用户运行，MemoryMax=120M
nginx:   location / -> 127.0.0.1:8090
```

资源文件放在控制台自己的目录而不是 `/etc/aisix`：原子替换需要**目录**写权限，
而把 `/etc/aisix` 开放给控制台会连带让它能替换 `aisix.env`（里面是管理密钥）。

## 本地开发

```sh
cargo run -p aisix-console --release -- hash '你的口令'   # 生成 argon2 散列
CONSOLE_PASSWORD_HASH='$argon2id$...' \
AISIX_ADMIN_KEY_FOR_CONSOLE=... \
AISIX_ADMIN_URL=http://127.0.0.1:8081 \
PROMETHEUS_URL=http://127.0.0.1:9090 \
AISIX_RESOURCES=/path/to/resources.yaml \
AISIX_BIN=/path/to/aisix \
CONSOLE_ADDR=127.0.0.1:8090 \
cargo run --release
```
