//! 用户自助管理自己的 API 密钥。
//!
//! # 额度挂在用户身上，不在密钥上
//!
//! 一个用户可以铸任意多把密钥，它们**共享同一份余额**（账本 + `scope: member`
//! 的限流策略）。所以「按密钥分额度」这件事这里不存在 —— 要拆预算就拆到用户
//! （或团队）一层，那是网关的作用域模型本来的分法。
//!
//! 由此推出一条不那么显然的实现要求：**新密钥在余额不足时必须生下来就是停用
//! 的。** 若先建成可用的，它会在网关眼里活到对账环下一轮（默认 15 秒）才被
//! 关掉 —— 那 15 秒是白送的推理，而且是每建一把密钥就送一次。
//!
//! # 明文只出现一次
//!
//! 落盘只有 sha256 散列（与网关 `key_hash` 的算法一致）。明文在铸出来的那一次
//! 响应里给出，此后任何接口都拿不到。

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rand_core::RngCore;
use serde::Deserialize;
use serde_json::json;
use serde_yaml_ng::Value;
use sha2::{Digest, Sha256};

use crate::auth::AppState;
use crate::ledger::Ledger;
use crate::resources::{self, WriteError};

/// 门户铸出来的密钥前缀。让运维一眼看出这把是用户自助建的，而不是手工建的。
const PREFIX: &str = "sk-aisix-";

#[derive(Deserialize)]
pub struct CreateReq {
    /// 用户给自己看的名字。空则用一个带序号的默认名。
    #[serde(default)]
    pub label: Option<String>,
}

/// `POST /api/keys` —— 铸一把新密钥。
pub async fn create(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateReq>,
) -> Response {
    let Some(uid) = st.session_user(&headers).await else {
        return unauthorized();
    };

    // 余额不足时新密钥直接建成停用态。见模块注释：否则每建一把就白送一轮
    // 对账周期的推理。
    let ledger = Ledger::new(st.store.clone());
    // 频率限制。放在写配置之前 —— 挡住的正是「循环铸密钥把网关按在重载上」。
    if !st.mint_allowed(&uid).await {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": "创建过于频繁，请稍后再试"})),
        )
            .into_response();
    }

    let born_disabled = ledger.balance(&uid).await.unwrap_or(0) <= 0;

    let plaintext = mint_plaintext();
    let hash = sha256_hex(&plaintext);
    // 展示名里带 uuid：它同时是删除时的标识，必须全局唯一，而用户填的 label
    // 是可以重名的。
    let name = format!(
        "portal-{}",
        uuid::Uuid::new_v4()
            .to_string()
            .split('-')
            .next()
            .unwrap_or("key")
    );
    let label = req
        .label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("我的密钥")
        .to_string();
    // 标签会原样进 `display_name`，也就是进网关的配置文件、进下推的策略名。
    // 不设上限的话，一个请求体大小的标签就能把配置撑起来，而网关每次重载都要
    // 把它整份读一遍。按字符数而不是字节数算，免得中文标签被误伤。
    if label.chars().count() > MAX_LABEL_CHARS {
        return bad("名称过长");
    }

    let uid2 = uid.clone();
    let hash2 = hash.clone();
    let name2 = name.clone();
    let label2 = label.clone();
    let allowed = st.default_allowed_models();
    let result = st
        .resources()
        .edit(move |doc| {
            let keys = resources::api_keys_mut(doc);
            let mut m = serde_yaml_ng::Mapping::new();
            m.insert(Value::from("display_name"), Value::from(name2.clone()));
            m.insert(Value::from("key_hash"), Value::from(hash2.clone()));
            m.insert(Value::from("user_id"), Value::from(uid2.clone()));
            m.insert(
                Value::from("allowed_models"),
                Value::Sequence(allowed.iter().map(|s| Value::from(s.clone())).collect()),
            );
            if born_disabled {
                m.insert(Value::from("disabled"), Value::from(true));
            }
            // label 存成网关不认识的自定义键会被 schema 拒掉，所以它只活在
            // 门户这一侧的 display_name 后缀里。
            m.insert(
                Value::from("display_name"),
                Value::from(format!("{name2} · {label2}")),
            );
            keys.push(Value::Mapping(m));
            true
        })
        .await;

    match result {
        Ok(_) => (
            StatusCode::CREATED,
            Json(json!({
                // 明文只在这一次出现。
                "plaintext": plaintext,
                "name": name,
                "label": label,
                "disabled": born_disabled,
                "note": if born_disabled {
                    Some("当前余额为零，这把密钥已建但处于停用态；管理员发放额度后会自动启用")
                } else {
                    None
                },
            })),
        )
            .into_response(),
        Err(e) => write_failed(e),
    }
}

/// `GET /api/keys` —— 列出本人的密钥。
pub async fn list(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(uid) = st.session_user(&headers).await else {
        return unauthorized();
    };
    let yaml = st.resources().read().await.unwrap_or_default();
    let rows = resources::list_keys(&yaml, &uid);
    let quotas = st.store.key_quotas(&uid).await.unwrap_or_default();
    let granted = Ledger::new(st.store.clone())
        .total_granted(&uid)
        .await
        .unwrap_or(0);
    let allocated: i64 = quotas.iter().map(|(_, v)| *v).sum();
    Json(json!({
        "granted_micro_usd": granted,
        "allocated_micro_usd": allocated,
        "keys": rows.iter().map(|r| json!({
            "name": r.display_name,
            "masked_hash": r.masked_hash,
            "disabled": r.disabled,
            // 0 = 没单独设限，只受用户总额度约束。
            "quota_micro_usd": quotas
                .iter()
                .find(|(k, _)| k == &r.display_name)
                .map(|(_, v)| *v)
                .unwrap_or(0),
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

/// `DELETE /api/keys/{name}` —— 吊销本人的一把密钥。
pub async fn revoke(
    State(st): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(uid) = st.session_user(&headers).await else {
        return unauthorized();
    };

    // 先把全名解析出来 —— 删额度要用它，见下面的说明。
    let yaml = st.resources().read().await.unwrap_or_default();
    let full_name = resources::list_keys(&yaml, &uid)
        .into_iter()
        .map(|k| k.display_name)
        .find(|d| d == &name || d.starts_with(&format!("{name} · ")))
        .unwrap_or_else(|| name.clone());

    let uid2 = uid.clone();
    let name2 = name.clone();
    let full2 = full_name.clone();
    let mut found = false;
    let seen = &mut found;
    let result = st
        .resources()
        .edit(|doc| {
            let keys = resources::api_keys_mut(doc);
            let before = keys.len();
            keys.retain(|k| {
                // **只能删自己的。** 少了 user_id 这一半，任何登录用户都能凭
                // 名字删掉别人的密钥 —— 包括运维那些没有 user_id 的。
                let mine = k.get("user_id").and_then(Value::as_str) == Some(uid2.as_str());
                let named = k
                    .get("display_name")
                    .and_then(Value::as_str)
                    .map(|d| d == name2 || d.starts_with(&format!("{name2} · ")))
                    .unwrap_or(false);
                !(mine && named)
            });
            *seen = keys.len() < before;
            if *seen {
                // **策略必须跟密钥同批走。**
                //
                // 撤策略原本交给下一轮对账，可那要等一个周期：中间这份文档里
                // 密钥行已经没了、`portal-key-*` 还指着它，网关会判成「引用了
                // 不存在的密钥」而**整份拒收**。也就是说吊销一把带额度的密钥会
                // 把整个配置冻住 —— 包括停用闸。写前校验会挡下这次写入，于是
                // 症状是吊销失败；不校验的话就是静默冻住。
                let want = format!("portal-key-{full}", full = full2);
                if let Some(list) = doc
                    .as_mapping_mut()
                    .and_then(|m| m.get_mut(Value::from("rate_limit_policies")))
                    .and_then(Value::as_sequence_mut)
                {
                    list.retain(|pol| {
                        pol.get("name").and_then(Value::as_str) != Some(want.as_str())
                    });
                }
            }
            *seen
        })
        .await;

    // **只在写成功之后动库。** 写盘失败时密钥仍在配置里，这时把额度记录删掉，
    // 下一轮对账就会撤掉它的策略 —— 那把密钥于此静默变成「不单独设限」。
    if found && result.is_ok() {
        // 留着的话下一轮对账还会为一把已经不存在的密钥下推策略。
        //
        // 按**完整 display_name** 删：额度是按全名存的（`portal-xxx · 标签`），
        // 而路径上给的是短名。用短名删会删不掉，而且不报错 —— 症状是吊销之后
        // 那份额度仍占着分配额，用户看着自己「还有额度却分不出去」。
        let _ = st.store.drop_key_quota(&uid, &full_name).await;
        // 累计花费也一起清。名字里带 uuid，同名密钥不会复用，留着只是垃圾。
        let _ = st.store.drop_key_spend(&uid, &full_name).await;
    }
    match result {
        Ok(_) if found => Json(json!({"ok": true})).into_response(),
        Ok(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "没有这把密钥"})),
        )
            .into_response(),
        Err(e) => write_failed(e),
    }
}

/// 密钥名称的长度上限（字符）。
const MAX_LABEL_CHARS: usize = 64;

/// 32 字节随机，hex 编码。熵足够，所以散列用 sha256 而不是 argon2 —— 后者是
/// 给低熵口令抗爆破用的，对随机串是白付代价。
fn mint_plaintext() -> String {
    let mut b = [0u8; 32];
    rand_core::OsRng.fill_bytes(&mut b);
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{PREFIX}{hex}")
}

pub fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|x| format!("{x:02x}")).collect()
}

#[derive(Deserialize)]
pub struct QuotaReq {
    /// 这把密钥的额度。0 = 不单独设限，只受用户总额度约束。
    /// 有意用 `i64`：负数要能进来才能被明确拒绝。
    pub micro_usd: i64,
}

/// `PUT /api/keys/{name}/quota` —— 给自己的一把密钥设额度。
///
/// # 不变量：各把密钥的额度之和 ≤ 用户总额度
///
/// 在这里校验，而不是指望调用方自律。校验用的是「其余密钥之和 + 这次的新值」，
/// 不是「当前总和 + 新值」—— 后者在**调低**某把密钥时会误判为超额。
///
/// 允许总和小于总额度：没分配出去的那部分仍然可用（用户级的闸还在），密钥级
/// 的额度是给用户自己做隔离用的，不是必须把总额分光。
pub async fn set_quota(
    State(st): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(req): Json<QuotaReq>,
) -> Response {
    let Some(uid) = st.session_user(&headers).await else {
        return unauthorized();
    };
    if req.micro_usd < 0 {
        return bad("额度不能是负数");
    }

    // 只能给自己的密钥设额度。
    let yaml = st.resources().read().await.unwrap_or_default();
    let mine = resources::list_keys(&yaml, &uid);
    let Some(full_name) = mine
        .iter()
        .map(|k| k.display_name.clone())
        .find(|d| d == &name || d.starts_with(&format!("{name} · ")))
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "没有这把密钥"})),
        )
            .into_response();
    };

    // **读额度 → 校验 → 写额度，一个事务。**
    //
    // 分三步各自跑的话，同一个用户对两把不同密钥的两次并发请求会各自读到同一份
    // 旧值、双双通过校验，于是各把额度之和越过用户总额 —— 正是这里要守的那条
    // 不变量被破掉。`BEGIN IMMEDIATE` 从第一条语句就拿写锁，第二个事务排队。
    let uid_tx = uid.clone();
    let name_tx = full_name.clone();
    let want = req.micro_usd;
    let outcome = st
        .store
        .immediate_tx(move |conn| {
            Box::pin(async move {
                let granted = crate::ledger::total_granted_on(&mut *conn, &uid_tx).await?;
                let quotas = crate::store::key_quotas_on(&mut *conn, &uid_tx).await?;
                let others: i64 = quotas
                    .iter()
                    .filter(|(k, _)| k != &name_tx)
                    .map(|(_, v)| *v)
                    .sum();
                // `checked_add`：`others + 新值` 会溢出。溢出在 release 下**回绕
                // 成负数**，于是这道上限被静默绕过，一个 i64::MAX 的额度就落了
                // 库；debug 下则是 panic。溢出本身就意味着「远超总额」，按超额处理。
                if others.checked_add(want).is_none_or(|v| v > granted) {
                    return Ok(Err((granted, others)));
                }
                if want == 0 {
                    crate::store::drop_key_quota_on(&mut *conn, &uid_tx, &name_tx).await?;
                } else {
                    crate::store::set_key_quota_on(&mut *conn, &uid_tx, &name_tx, want).await?;
                }
                Ok(Ok(()))
            })
        })
        .await;

    let r = match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err((granted, others))) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "各把密钥的额度之和不能超过你的总额度",
                    "granted_micro_usd": granted,
                    "other_keys_micro_usd": others,
                    "available_micro_usd": granted - others,
                })),
            )
                .into_response()
        }
        Err(e) => Err(e),
    };
    match r {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(_) => server_error("写入失败"),
    }
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({"error": "未登录"}))).into_response()
}

fn bad(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response()
}

fn server_error(msg: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": msg})),
    )
        .into_response()
}

/// 写配置失败时给调用方的应答。
///
/// **细节只进日志，不进响应体。** `WouldBreakGateway` 的原文来自网关的加载器，
/// 它在报「引用了不存在的密钥」时会把**文件里所有已知密钥名**列出来做提示 ——
/// 那里面有别的租户的密钥名；`Io` 的原文里带着服务端的文件路径。两者都不该出现
/// 在一个租户能看到的地方。运维要的那份细节写到 stderr。
fn write_failed(e: WriteError) -> Response {
    let (code, msg) = match &e {
        WriteError::Contended => (StatusCode::CONFLICT, "配置正被改写，请重试"),
        WriteError::WouldBreakGateway(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "这次改动没有生效，请联系管理员",
        ),
        WriteError::Io(_) | WriteError::Parse(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "配置读写失败")
        }
    };
    eprintln!("门户写配置失败: {e}");
    (code, Json(json!({"error": msg}))).into_response()
}

#[cfg(test)]
mod error_body_tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_of(r: Response) -> String {
        String::from_utf8_lossy(&to_bytes(r.into_body(), 64 * 1024).await.unwrap()).to_string()
    }

    /// 写配置失败的应答里不能带上加载器原文。
    ///
    /// 那段原文在报「引用了不存在的密钥」时会把文件里**所有**已知密钥名列出来
    /// 当提示 —— 里面有别的租户的密钥名。这是一个租户能看到的响应体。
    #[tokio::test]
    async fn 写配置失败的应答不泄漏别人的密钥名与服务端路径() {
        let leaky = "resources file /etc/aisix/resources.yaml: 1 error(s):\n               - rate_limit_policies[1]: `scope_ref` references unknown api key \"portal-aaa · 我的\"              (defined api_keys: default, portal-bbb · 别人的密钥, portal-ccc · 又一个人的)";
        let r = write_failed(WriteError::WouldBreakGateway(leaky.to_string()));
        assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_of(r).await;
        for secret in ["portal-bbb", "别人的密钥", "portal-ccc", "/etc/aisix"] {
            assert!(!body.contains(secret), "应答里泄漏了 {secret}: {body}");
        }

        let r = write_failed(WriteError::Io(
            "写入 /var/lib/aisix-console/resources.yaml 失败: 权限不足".into(),
        ));
        let body = body_of(r).await;
        assert!(!body.contains("/var/lib"), "应答里泄漏了服务端路径: {body}");

        // 冲突这一类本来就没有敏感内容，仍然要能被区分出来（409，可以重试）。
        let r = write_failed(WriteError::Contended);
        assert_eq!(r.status(), StatusCode::CONFLICT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 明文带前缀且熵足够() {
        let a = mint_plaintext();
        let b = mint_plaintext();
        assert!(a.starts_with(PREFIX));
        assert_ne!(a, b, "两次铸出了同一把密钥");
        assert_eq!(a.len(), PREFIX.len() + 64);
    }

    #[test]
    fn 散列与网关的算法一致() {
        // 网关的 key_hash 是明文的 sha256 十六进制。对不上的话密钥永远认不出来。
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
