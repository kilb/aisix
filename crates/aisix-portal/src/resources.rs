//! 读网关的 `resources.yaml`。
//!
//! 一期只读（数密钥）。Task 7 的控制环要在这里加写入 —— 用与控制台相同的内容
//! 散列乐观并发，因为两个进程会写同一个文件（计划裁决 2 的代价，二期换成
//! Admin API 写入后消失）。

use serde::Deserialize;

/// 只取门户关心的那几个字段。其余配置原样留在文件里，不经过这个结构体，
/// 免得门户变成一份必须跟网关模型同步的影子定义。
#[derive(Debug, Deserialize)]
struct Doc {
    #[serde(default)]
    api_keys: Vec<KeyRow>,
}

#[derive(Debug, Deserialize)]
struct KeyRow {
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    disabled: bool,
}

/// 某个用户名下的密钥数（总数，以及其中已停用的）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyTally {
    pub total: usize,
    pub disabled: usize,
}

/// 数出携带该 `user_id` 的密钥。
///
/// 这个计数存在的理由是把一类静默失败变成可见状态：一期的密钥由管理员手工
/// 创建并填 `user_id`，填错一个字符网关照常放行、指标打错标签、门户查不到
/// 用量 —— 屏幕上「用量一直是 0」跟「还没开始用」完全一样，而它实际意味着
/// 这个人在免费用。数出 0 就明确说未绑定。
pub fn tally_keys(yaml: &str, user_id: &str) -> KeyTally {
    let doc: Doc = match serde_yaml_ng::from_str(yaml) {
        Ok(d) => d,
        // 读不动就报 0 —— 与「确实没有密钥」同一个显示，但那是保守方向：
        // 它促使人去看，而不是让人以为一切正常。
        Err(_) => {
            return KeyTally {
                total: 0,
                disabled: 0,
            }
        }
    };
    let mine: Vec<&KeyRow> = doc
        .api_keys
        .iter()
        .filter(|k| k.user_id.as_deref() == Some(user_id))
        .collect();
    KeyTally {
        total: mine.len(),
        disabled: mine.iter().filter(|k| k.disabled).count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"
api_keys:
- display_name: mine-a
  key_hash: aa
  user_id: u1
- display_name: mine-b
  key_hash: bb
  user_id: u1
  disabled: true
- display_name: someone-else
  key_hash: cc
  user_id: u2
- display_name: unowned
  key_hash: dd
"#;

    #[tokio::test]
    async fn 只数属于本人的密钥() {
        let t = tally_keys(DOC, "u1");
        assert_eq!(t.total, 2);
        assert_eq!(t.disabled, 1);
    }

    #[tokio::test]
    async fn 没有密钥携带该_user_id_时数出零() {
        assert_eq!(tally_keys(DOC, "nobody").total, 0);
    }

    #[tokio::test]
    async fn 没有主人的密钥不算给任何人() {
        // `user_id` 缺失的密钥是运维自己用的，不该被算进某个用户名下。
        let all: usize = ["u1", "u2", "nobody"]
            .iter()
            .map(|u| tally_keys(DOC, u).total)
            .sum();
        assert_eq!(all, 3, "有密钥被算给了不该算的人");
    }
}
