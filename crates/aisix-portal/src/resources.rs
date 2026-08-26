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
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contended => write!(f, "配置文件正被其它进程改写，请重试"),
            Self::Io(e) => write!(f, "读写配置失败: {e}"),
            Self::Parse(e) => write!(f, "配置不是合法 YAML: {e}"),
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
            if !edit(&mut doc) {
                return Ok(false);
            }
            // 进程外的写入者（控制台）可能刚好插在中间。
            if self.read().await? != before {
                continue;
            }
            let out =
                serde_yaml_ng::to_string(&doc).map_err(|e| WriteError::Parse(e.to_string()))?;
            tokio::fs::write(&*self.path, out)
                .await
                .map_err(|e| WriteError::Io(e.to_string()))?;
            signal_reload().await;
            return Ok(true);
        }
        Err(WriteError::Contended)
    }
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
pub fn api_keys_mut(doc: &mut Value) -> &mut Vec<Value> {
    let map = doc.as_mapping_mut().expect("配置根不是映射");
    let k = Value::from("api_keys");
    if !map.get(&k).map(Value::is_sequence).unwrap_or(false) {
        map.insert(k.clone(), Value::Sequence(Vec::new()));
    }
    map.get_mut(&k)
        .and_then(Value::as_sequence_mut)
        .expect("api_keys 刚被建成序列")
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
