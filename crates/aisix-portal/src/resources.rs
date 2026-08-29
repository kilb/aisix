//! 网关 `resources.yaml` 的读写。
//!
//! # 为什么读改写要串行
//!
//! 门户有两条写这个文件的路径：用户铸密钥、对账环停用/启用密钥。两者都是
//! 「读全文 → 改一处 → 写回」，不串行就会互相覆盖 —— 后写的那次带着自己读到
//! 的旧全文，把中间那次改动整段抹掉。所以两条路径共用同一个 [`Writer`]。
//!
//! 进程外还有一个写入者（管理控制台）。那个用内容比对处理：写之前确认文件
//! 与读到时一致，不一致就重来。这挡不住 TOCTOU 的最后一瞬，但一期的写入频率
//! 极低（铸密钥、余额归零），而二期换成 Admin API 写 etcd 之后整个问题消失。
//!
//! # 为什么改动走泛型 Value
//!
//! 绝不能把整份配置反序列化进门户认识的窄结构体再写回去 —— 那会把网关配置里
//! 门户不认识的字段全部抹掉（模型、供应商、限流策略、守卫……）。
//!
//! # 注释保不住
//!
//! 泛型 `Value` 保住了所有**字段**，但 YAML 注释不是字段：反序列化再序列化
//! 会把它们全部吃掉。这在生产上真的发生过一次 —— 那份 `resources.yaml` 顶上
//! 有十几行注释掉的示例（怎么加 provider_keys、怎么用 `${VAR}` 从 env 读密钥），
//! 门户第一次铸密钥就把它们清空了，而且不报错。
//!
//! 修不掉：要保注释得做定点文本编辑，对任意 YAML 正确实现的代价远超收益。
//! 所以改成让这件事**在每次重写之后仍然可见** —— [`BANNER`] 会被重新写到文件
//! 顶部，明确告诉下一个打开它的人：这里的注释留不住，说明写到别处去。

use std::sync::Arc;

use serde_yaml_ng::Value;
use tokio::sync::Mutex;

/// 某个用户名下的密钥数（总数，以及其中已停用的）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyTally {
    pub total: usize,
    pub disabled: usize,
}

/// 列表里的一把密钥。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRow {
    /// 展示名，也是删除时的标识 —— 门户建的名字里带 uuid，全局唯一。
    pub display_name: String,
    /// 散列的两端，中间遮住。明文只在铸出来那一次出现过。
    pub masked_hash: String,
    pub disabled: bool,
}

/// 每次重写后放回文件顶部的说明。
///
/// 序列化会吃掉文件里所有注释，包括上一次写的这段横幅，所以每次都要重新写。
/// 它存在的意义是：让「这个文件由机器改写、注释留不住」这件事在文件自己身上
/// 说出来，而不是指望运维记得。
const BANNER: &str = "\
# 这个文件由机器改写：控制台保存配置、门户铸密钥与停用密钥都会重写它。\n\
# 重写走 YAML 反序列化再序列化，**注释不会保留**。要写给人看的说明，请放在\n\
# 同目录的 resources.README 里，不要放在这个文件里。\n\
\n";

/// 独占写这个文件的闸口。
#[derive(Clone)]
pub struct Writer {
    path: Arc<String>,
    /// 进程内串行。两条写路径共用一把锁，见模块注释。
    lock: Arc<Mutex<()>>,
}

#[derive(Debug)]
pub enum WriteError {
    /// 读改写之间文件被进程外改过，重试若干次仍然如此。
    Contended,
    Io(String),
    Parse(String),
    /// 这次改动生成的文档网关加载不了。**没有写盘。**
    WouldBreakGateway(String),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contended => write!(f, "配置文件正被其它进程改写，请重试"),
            Self::Io(e) => write!(f, "读写配置失败: {e}"),
            Self::Parse(e) => write!(f, "配置不是合法 YAML: {e}"),
            Self::WouldBreakGateway(e) => {
                write!(f, "这次改动会让网关拒收整份配置，已放弃写入: {e}")
            }
        }
    }
}

impl Writer {
    pub fn new(path: String) -> Self {
        Self {
            path: Arc::new(path),
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub async fn read(&self) -> Result<String, WriteError> {
        tokio::fs::read_to_string(&*self.path)
            .await
            .map_err(|e| WriteError::Io(e.to_string()))
    }

    /// 读改写。`edit` 拿到整份文档的泛型 `Value`，返回是否真的改了。
    ///
    /// 改了才写盘、才发 SIGHUP —— 每轮对账都无条件重写文件会让 mtime 一直动，
    /// 也会让网关白重载。
    pub async fn edit<F>(&self, mut edit: F) -> Result<bool, WriteError>
    where
        F: FnMut(&mut Value) -> bool,
    {
        let _guard = self.lock.lock().await;
        for _ in 0..5 {
            let before = self.read().await?;
            let mut doc: Value =
                serde_yaml_ng::from_str(&before).map_err(|e| WriteError::Parse(e.to_string()))?;
            // **根必须是映射，在调闭包之前挡住。**
            //
            // 空文件解析成 `Null` 而不是报错，而闭包里的 `api_keys_mut` 会对
            // 非映射的根 panic。调用方有两类：用户请求（铸密钥、吊销）panic 是
            // 一次 500；**对账环**里 panic 会把那个 spawn 出去的任务整个杀掉 ——
            // 计费从此停摆，没有任何东西会说出来，直到有人重启门户。
            //
            // 挡在这里而不是让每个闭包各自小心：闭包有三个，写第四个的人不会
            // 想到这件事。
            if !doc.is_mapping() {
                return Err(WriteError::Parse(
                    "配置的根不是映射（空文件也算）——不敢在它上面改东西".into(),
                ));
            }
            if !edit(&mut doc) {
                return Ok(false);
            }
            // 进程外的写入者（控制台）可能刚好插在中间。
            if self.read().await? != before {
                continue;
            }
            let body =
                serde_yaml_ng::to_string(&doc).map_err(|e| WriteError::Parse(e.to_string()))?;
            // 横幅重新写回顶部：序列化吃掉了所有注释，包括上一次的横幅。
            let out = format!("{BANNER}{body}");
            if let Some(why) = breaks_gateway(&before, &out) {
                return Err(WriteError::WouldBreakGateway(why));
            }
            write_atomic(&self.path, &out).await?;
            signal_reload().await;
            return Ok(true);
        }
        Err(WriteError::Contended)
    }
}

/// 先写同目录的临时文件，再 rename 覆盖。
///
/// `tokio::fs::write` 是「截断 + 写」：进程在中间死掉就留下一个截断的配置。网关
/// 下次启动会直接加载失败 —— 那是整站不可用，而不是某条策略不生效。同一个文件
/// 系统上的 rename 是原子的，读者要么看到旧的完整内容、要么看到新的。
///
/// 临时文件放在**同目录**：跨文件系统 rename 会失败。写完先 fsync，否则崩溃后
/// 可能 rename 已生效而内容还没落盘。
async fn write_atomic(path: &str, body: &str) -> Result<(), WriteError> {
    // tokio 的 OpenOptions 自带 mode()，不需要 OpenOptionsExt。
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncWriteExt;

    let tmp = format!("{path}.tmp-{}", uuid::Uuid::new_v4());
    let io = |e: std::io::Error| WriteError::Io(e.to_string());
    // **建的时候就带上正确权限**，不是建完再改。
    //
    // 沿用原文件的权限（rename 会把临时文件的权限一并带过去，所以运维设的 0600
    // 不能被降级）；原文件还不存在时默认 0600 —— 这份内容里有密钥散列，按 umask
    // 建成 0644 等于第一次写就把它摊开。控制台那条路径早就是这么做的。
    //
    // 先建后改也能得到最终权限，但中间有一小段窗口文件是 0644 的。
    let mode = tokio::fs::metadata(path)
        .await
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0o600);
    let mut f = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .await
        .map_err(io)?;
    let r = async {
        f.write_all(body.as_bytes()).await.map_err(io)?;
        // rename 之前落盘：崩在写与 rename 之间会让网关读到半截文件。
        f.sync_all().await.map_err(io)?;
        drop(f);
        tokio::fs::rename(&tmp, path).await.map_err(io)
    }
    .await;
    if r.is_err() {
        // 失败了就别把临时文件留在配置目录里。
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    r
}

/// 这次改动会不会让网关拒收整份配置？会的话返回原因。
///
/// 用网关**自己**那个加载器来判，不是另写一份校验 —— 另写一份就一定会跟它漂移。
///
/// 为什么必须有这一道：文件模式下加载失败时网关只记一条 WARN、**保留旧快照**，
/// 门户这边一无所知。于是一个字段写错的后果不是「这条策略不生效」，而是从那一刻
/// 起整份配置都冻住 —— 包括「余额耗尽就停用」那个闸。这真的发生过一次：`scope_ref`
/// 写成了派生 id 而不是密钥名字，网关把它当成引用了不存在的密钥，整份配置拒收。
///
/// 判据是「**是我这次改动弄坏的**」，而不是「文档能不能加载」：一份本来就坏掉的
/// 文件（运维自己写错了）不该让门户从此拒绝写入 —— 那会把停用闸一起卡死，比原来
/// 的问题更糟。
///
/// `${VAR}` 一律插成占位串，而不是查门户自己的环境变量：这里要验的是**结构与
/// 引用**，而门户与网关的环境本来就可以不同，拿门户的 env 去判会把「网关那边有
/// 这个变量」误判成致命错。
fn breaks_gateway(before: &str, after: &str) -> Option<String> {
    let load = |body: &str| {
        aisix_core::filesource::load_from_str(body, "portal-candidate", 1, &|_| {
            Some("placeholder".to_string())
        })
    };
    let err = match load(after) {
        Ok(_) => return None,
        Err(e) => e,
    };
    if load(before).is_err() {
        // 本来就坏着 —— 不是这次改动的问题，照写。
        return None;
    }
    Some(err.to_string())
}

/// 写完配置后让网关重载。
///
/// **不做这一步的话所有改动都是空的。** 文件模式下网关只在 SIGHUP 时重新读
/// `resources_file`（`crates/aisix-server/src/main.rs` 的 SIGHUP 处理，没有文件
/// 监听）。少了信号，门户会尽职地写下 `disabled: true`、账面显示负余额，而客户
/// 被无限期继续服务 —— 一个静默的免费推理洞。铸出来的新密钥同理：文件里有了，
/// 网关不认，用户拿到一把用不了的密钥。
///
/// 做法与 `crates/aisix-console` 一致：`pkill -HUP -x aisix`。二期改走 Admin
/// API 写 etcd 之后，watch 会自动传播，这一步随之消失。
async fn signal_reload() {
    let out = tokio::process::Command::new("pkill")
        .args(["-HUP", "-x", "aisix"])
        .output()
        .await;
    // 配置已经落盘。信号没送到只是「还没热加载」，不该当成写入失败 ——
    // 但必须报出来，否则这件事无处可查。
    if !matches!(&out, Ok(o) if o.status.success()) {
        eprintln!("已改写 resources.yaml，但 SIGHUP 未送达：网关仍在用旧配置");
    }
}

/// 取出 `api_keys` 序列（不存在则就地建一个空的）。
///
/// 只能在 [`Writer::edit`] 的闭包里调用 —— 那里已经确认过根是映射。别处调用
/// 要自己先确认，否则这里会 panic。
pub fn api_keys_mut(doc: &mut Value) -> &mut Vec<Value> {
    let map = doc
        .as_mapping_mut()
        .expect("根不是映射；Writer::edit 已在调用闭包前挡住这种文档");
    let k = Value::from("api_keys");
    if !map.get(&k).map(Value::is_sequence).unwrap_or(false) {
        map.insert(k.clone(), Value::Sequence(Vec::new()));
    }
    map.get_mut(&k)
        .and_then(Value::as_sequence_mut)
        .expect("api_keys 刚被建成序列")
}

/// 文件里现存的所有密钥名。**解析不了时返回 `None`，不是空表。**
///
/// 这个区别是关键：调用方拿它去剪除「已不存在的密钥」占着的额度，把一次读取
/// 失败当成「一把密钥都没有」，会把所有人的额度分配一次清空。
pub fn all_key_names(yaml: &str) -> Option<Vec<String>> {
    let doc: Value = serde_yaml_ng::from_str(yaml).ok()?;
    // **看不懂的文档要回 `None`，不能回「一把密钥都没有」。**
    //
    // 调用方拿它去剪除「已不存在的密钥」占的额度，把「读到一份看不懂的东西」
    // 当成空表，会把所有人的额度分配一次清空 —— 而这是可达的：写盘不是原子的，
    // 写到一半崩溃就留下一个空文件，`from_str("")` 解析成 `Null` 而不是报错。
    // 同理，`api_keys` 存在但不是序列，说明这份文档不是我们认识的形状。
    let map = doc.as_mapping()?;
    match map.get(Value::from("api_keys")) {
        // 没有这一段：合法的「确实没有密钥」。
        None => Some(Vec::new()),
        Some(v) => v.as_sequence().map(|s| {
            s.iter()
                .filter_map(|k| k.get("display_name").and_then(Value::as_str))
                .map(String::from)
                .collect()
        }),
    }
}

fn owner(k: &Value) -> Option<&str> {
    k.get("user_id").and_then(Value::as_str)
}

fn is_disabled(k: &Value) -> bool {
    k.get("disabled").and_then(Value::as_bool).unwrap_or(false)
}

/// 数出携带该 `user_id` 的密钥。
///
/// 这个计数存在的理由是把一类静默失败变成可见状态：管理员手工创建密钥时填错
/// `user_id`，网关照常放行、指标打错标签、门户查不到用量 —— 屏幕上「用量一直
/// 是 0」跟「还没开始用」完全一样，而它实际意味着这个人在免费用。
pub fn tally_keys(yaml: &str, user_id: &str) -> KeyTally {
    let Ok(doc) = serde_yaml_ng::from_str::<Value>(yaml) else {
        // 读不动就报 0 —— 与「确实没有密钥」同一个显示，但那是保守方向：
        // 它促使人去看，而不是让人以为一切正常。
        return KeyTally {
            total: 0,
            disabled: 0,
        };
    };
    let mine: Vec<&Value> = doc
        .get("api_keys")
        .and_then(Value::as_sequence)
        .map(|s| s.iter().filter(|k| owner(k) == Some(user_id)).collect())
        .unwrap_or_default();
    KeyTally {
        total: mine.len(),
        disabled: mine.iter().filter(|k| is_disabled(k)).count(),
    }
}

/// 列出某个用户的密钥。散列遮成两端 —— 明文只在铸出来那一次出现过。
pub fn list_keys(yaml: &str, user_id: &str) -> Vec<KeyRow> {
    let Ok(doc) = serde_yaml_ng::from_str::<Value>(yaml) else {
        return Vec::new();
    };
    doc.get("api_keys")
        .and_then(Value::as_sequence)
        .map(|s| {
            s.iter()
                .filter(|k| owner(k) == Some(user_id))
                .map(|k| KeyRow {
                    display_name: k
                        .get("display_name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    masked_hash: mask(k.get("key_hash").and_then(Value::as_str).unwrap_or("")),
                    disabled: is_disabled(k),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 只留两端。全长散列本身不是凭据，但也没有任何理由发出去。
fn mask(h: &str) -> String {
    if h.len() <= 12 {
        return "…".into();
    }
    format!("{}…{}", &h[..6], &h[h.len() - 6..])
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"
models:
- display_name: keep-me
api_keys:
- display_name: mine-a
  key_hash: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  user_id: u1
- display_name: mine-b
  key_hash: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
  user_id: u1
  disabled: true
- display_name: someone-else
  key_hash: cccccccccccccccccccccccccccccccc
  user_id: u2
- display_name: unowned
  key_hash: dddddddddddddddddddddddddddddddd
"#;

    #[test]
    fn 只数属于本人的密钥() {
        let t = tally_keys(DOC, "u1");
        assert_eq!(t.total, 2);
        assert_eq!(t.disabled, 1);
    }

    #[test]
    fn 没有密钥携带该_user_id_时数出零() {
        assert_eq!(tally_keys(DOC, "nobody").total, 0);
    }

    #[test]
    fn 没有主人的密钥不算给任何人() {
        // `user_id` 缺失的密钥是运维自己用的，不该被算进某个用户名下。
        let all: usize = ["u1", "u2", "nobody"]
            .iter()
            .map(|u| tally_keys(DOC, u).total)
            .sum();
        assert_eq!(all, 3, "有密钥被算给了不该算的人");
    }

    #[test]
    fn 列表只给本人的密钥_且散列被遮蔽() {
        let rows = list_keys(DOC, "u1");
        assert_eq!(rows.len(), 2);
        for r in &rows {
            // 全长散列没有理由发出去。
            assert!(r.masked_hash.contains('…'), "{r:?}");
            assert!(r.masked_hash.len() < 20, "{r:?}");
        }
        assert!(rows.iter().any(|r| r.disabled));
    }

    #[test]
    fn 配置里没有_api_keys_时就地建一个空序列() {
        let mut doc: Value = serde_yaml_ng::from_str("models: []\n").unwrap();
        assert!(api_keys_mut(&mut doc).is_empty());
        // 而且不能把 models 弄丢。
        assert!(doc.get("models").is_some());
    }
}

#[cfg(test)]
mod banner_tests {
    use super::*;

    async fn writer_with(body: &str) -> (Writer, String) {
        let path = std::env::temp_dir()
            .join(format!("aisix-banner-{}.yaml", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .to_string();
        std::fs::write(&path, body).unwrap();
        (Writer::new(path.clone()), path)
    }

    #[tokio::test]
    async fn 每次重写都把横幅放回顶部() {
        let (w, path) = writer_with("api_keys: []\n").await;
        w.edit(|doc| {
            api_keys_mut(doc).push(serde_yaml_ng::Value::from("x"));
            true
        })
        .await
        .unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.starts_with("# 这个文件由机器改写"), "{after}");
        // 序列化会吃掉上一次的横幅，所以第二次写也必须放回去，且只有一份。
        w.edit(|doc| {
            api_keys_mut(doc).push(serde_yaml_ng::Value::from("y"));
            true
        })
        .await
        .unwrap();
        let twice = std::fs::read_to_string(&path).unwrap();
        assert_eq!(twice.matches("这个文件由机器改写").count(), 1, "{twice}");
    }

    #[tokio::test]
    async fn 带横幅的文件仍然能被解析回来() {
        let (w, path) = writer_with("api_keys: []\n").await;
        w.edit(|doc| {
            api_keys_mut(doc).push(serde_yaml_ng::Value::from("x"));
            true
        })
        .await
        .unwrap();
        // 横幅是注释，不能让下一轮读改写读不动这个文件。
        let ok = w
            .edit(|doc| {
                assert_eq!(api_keys_mut(doc).len(), 1);
                false
            })
            .await;
        assert!(ok.is_ok(), "{ok:?}");
        let _ = std::fs::remove_file(path);
    }
}

/// 写前校验：这次改动会不会让网关拒收整份配置。
#[cfg(test)]
mod guard_tests {
    use super::*;

    /// 一份网关真能加载的配置。残缺文档会让每个用例都走「本来就坏」那条降级
    /// 路径，闸就等于没测。
    const VALID: &str = r#"_format_version: "1"
provider_keys:
  - display_name: stub
    provider: openai
    api_key: sk-stub
models:
  - display_name: m
    provider: openai
    provider_key: stub
    model_name: m
api_keys:
  - display_name: k
    key_hash: aa
    allowed_models: ["*"]
"#;

    async fn writer_with(body: &str) -> (Writer, String) {
        let path = std::env::temp_dir()
            .join(format!("aisix-guard-{}.yaml", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .to_string();
        std::fs::write(&path, body).unwrap();
        (Writer::new(path.clone()), path)
    }

    /// 往文档里塞一条引用了不存在密钥的 api_key 域策略 —— 正是踩过的那个坑。
    fn add_bogus_policy(doc: &mut Value) -> bool {
        let map = doc.as_mapping_mut().unwrap();
        let mut m = serde_yaml_ng::Mapping::new();
        m.insert(Value::from("name"), Value::from("bogus"));
        m.insert(Value::from("scope"), Value::from("api_key"));
        m.insert(
            Value::from("scope_ref"),
            Value::from("6efd592d-a73b-573d-8e2c-e9e103692f92"),
        );
        m.insert(Value::from("granted_micro_usd"), Value::from(1_000_000));
        map.insert(
            Value::from("rate_limit_policies"),
            Value::Sequence(vec![Value::Mapping(m)]),
        );
        true
    }

    /// 看不懂的文档必须回 `None`，不能回「一把密钥都没有」。
    ///
    /// 回空表的话，调用方会把所有人的密钥额度当成残留一次清空。空文件是可达的：
    /// 写盘不是原子的，写到一半崩溃就留下它，而 `from_str("")` 不报错。
    /// 写盘是原子的：读者要么看到旧的完整内容，要么看到新的。
    ///
    /// 「截断 + 写」在进程中途死掉时会留下一个截断的配置，网关下次启动直接加载
    /// 失败 —— 那是整站不可用，不是某条策略不生效。这条一边狂写一边狂读，读到
    /// 的每一份都必须是完整的两者之一。
    #[tokio::test]
    async fn 写盘是原子的_读不到写了一半的样子() {
        let (w, path) = writer_with(VALID).await;
        let orig = std::fs::read_to_string(&path).unwrap();
        let p2 = path.clone();
        let reader = tokio::task::spawn_blocking(move || {
            let mut bad = 0;
            for _ in 0..3_000 {
                let text = std::fs::read_to_string(&p2).unwrap_or_default();
                // 每一次读到的都得是一份能解析、且带 api_keys 的完整文档。
                if super::all_key_names(&text).is_none() {
                    bad += 1;
                }
            }
            bad
        });
        for i in 0..40 {
            w.edit(move |d| {
                let keys = api_keys_mut(d);
                keys[0]
                    .as_mapping_mut()
                    .unwrap()
                    .insert(Value::from("display_name"), Value::from(format!("k{i}")));
                true
            })
            .await
            .unwrap();
        }
        assert_eq!(reader.await.unwrap(), 0, "读到过写了一半的配置");
        assert_ne!(std::fs::read_to_string(&path).unwrap(), orig);
        let _ = std::fs::remove_file(path);
    }

    /// rename 覆盖不能把原文件的权限放宽。
    ///
    /// 新建的临时文件默认 0644，直接 rename 过去会让一份 0600 的配置变成人人可读
    /// —— 那里面有密钥散列。
    #[tokio::test]
    async fn 覆盖写不会放宽文件权限() {
        use std::os::unix::fs::PermissionsExt;
        let (w, path) = writer_with(VALID).await;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        w.edit(|d| {
            api_keys_mut(d)[0]
                .as_mapping_mut()
                .unwrap()
                .insert(Value::from("disabled"), Value::from(true));
            true
        })
        .await
        .unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "权限被放宽成了 {mode:o}");
        let _ = std::fs::remove_file(path);
    }

    /// 根不是映射的配置（空文件就是这种）不能让写入路径 panic。
    ///
    /// 这个闭包跑在 `edit` 里，而 `edit` 的调用方有两类：用户请求（铸密钥、
    /// 吊销）和**对账环**。前者 panic 是一次 500，后者 panic 会把那个 spawn 出去
    /// 的任务整个杀掉 —— 计费从此停摆，没有任何东西会说出来，直到有人重启门户。
    #[tokio::test]
    async fn 根不是映射时报错而不是崩掉() {
        for body in ["", "- 1\n- 2\n", "just a string\n"] {
            let (w, path) = writer_with(body).await;
            let r = w
                .edit(|d| {
                    api_keys_mut(d).push(Value::from("x"));
                    true
                })
                .await;
            assert!(
                matches!(r, Err(WriteError::Parse(_))),
                "{body:?} 应当报错，实际: {r:?}"
            );
            let _ = std::fs::remove_file(path);
        }
    }

    /// 目标文件还不存在时，新写出的那份必须是 0600。
    ///
    /// 这份内容里有密钥散列。按 umask 建成 0644 等于第一次写就把它摊开，而且
    /// 之后每次 rename 都会把这个权限延续下去 —— 一次疏忽会一直留着。
    #[tokio::test]
    async fn 文件不存在时新写出的配置是_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("aisix-first-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("resources.yaml").to_string_lossy().to_string();
        // 目标不存在。`edit` 会先读，读不到就直接报错，所以这里调底层的写。
        super::write_atomic(&path, "api_keys: []\n").await.unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "新写出的配置权限是 {mode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 看不懂的配置不能被当成没有密钥() {
        assert_eq!(
            all_key_names("api_keys:\n- display_name: a\n"),
            Some(vec!["a".to_string()])
        );
        // 合法的「确实没有密钥」。
        assert_eq!(all_key_names("models: []\n"), Some(Vec::new()));
        assert_eq!(all_key_names("api_keys: []\n"), Some(Vec::new()));

        // 看不懂的几种。
        for junk in [
            "",              // 空文件 —— 写到一半崩溃就是这个
            "api_keys: 3\n", // 不是序列
            "api_keys: [\n", // 不是合法 YAML
            "- 1\n- 2\n",    // 根不是映射
            "just a string\n",
        ] {
            assert_eq!(all_key_names(junk), None, "{junk:?} 被当成了「没有密钥」");
        }
    }

    #[tokio::test]
    async fn 会让网关拒收整份配置的改动不写盘() {
        let (w, path) = writer_with(VALID).await;
        let before = std::fs::read_to_string(&path).unwrap();

        let r = w.edit(add_bogus_policy).await;
        assert!(
            matches!(r, Err(WriteError::WouldBreakGateway(_))),
            "坏文档被放过去了: {r:?}"
        );
        // 关键是文件没动。写下去的后果不是「这条策略不生效」，而是网关从此
        // 保留旧快照 —— 连停用闸也一起冻住，而且只有一条 WARN。
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn 本来就加载不了的文件不会因此拒绝写入() {
        // 运维自己把文件写坏了。这时候门户若拒绝写入，「余额耗尽就停用」也跟着
        // 停摆 —— 比原来的问题更糟。判据必须是「是我这次改动弄坏的」。
        let (w, path) = writer_with("api_keys: []\n").await;
        let r = w
            .edit(|doc| {
                api_keys_mut(doc).push(Value::from("x"));
                true
            })
            .await;
        assert!(r.is_ok(), "本来就坏的文件让门户彻底写不动了: {r:?}");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn 门户环境里少个变量_不会把这道闸悄悄关掉() {
        // 生产上 `${VAR}` 是从环境读上游密钥的正常写法，而门户与网关的环境本来
        // 可以不同。
        //
        // 若拿门户自己的 env 去插值：这份配置在门户看来「本来就加载不了」，于是
        // 上面那条「本来就坏就照写」的出口每次都命中 —— 闸被悄悄关掉，坏文档照
        // 样写下去。这是比误拒更隐蔽的失效：什么都不报，只是不再拦。
        //
        // 用身份字段而不是普通字段：普通字段上未设的变量只展开成空串，分不出两
        // 种插值方式；身份字段上未设值是明确的加载错误。
        assert!(std::env::var("AISIX_PORTAL_GUARD_UNSET").is_err());
        let doc = VALID.replace(
            "  - display_name: k\n",
            "  - display_name: \"${AISIX_PORTAL_GUARD_UNSET}\"\n",
        );
        let (w, path) = writer_with(&doc).await;
        let before = std::fs::read_to_string(&path).unwrap();

        let r = w.edit(add_bogus_policy).await;
        assert!(
            matches!(r, Err(WriteError::WouldBreakGateway(_))),
            "配置里带 ${{VAR}} 就把闸关掉了: {r:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let _ = std::fs::remove_file(path);
    }
}
