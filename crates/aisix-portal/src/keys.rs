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
            *seen
        })
        .await;

    if found {
        // 留着的话下一轮对账还会为一把已经不存在的密钥下推策略。
        //
        // 按**完整 display_name** 删：额度是按全名存的（`portal-xxx · 标签`），
        // 而路径上给的是短名。用短名删会删不掉，而且不报错 —— 症状是吊销之后
        // 那份额度仍占着分配额，用户看着自己「还有额度却分不出去」。
        let _ = st.store.drop_key_quota(&uid, &full_name).await;
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

    let ledger = Ledger::new(st.store.clone());
    let (Ok(granted), Ok(quotas)) = (
        ledger.total_granted(&uid).await,
        st.store.key_quotas(&uid).await,
    ) else {
        return server_error("读取失败");
    };
    let others: i64 = quotas
        .iter()
        .filter(|(k, _)| k != &full_name)
        .map(|(_, v)| *v)
        .sum();
    if others + req.micro_usd > granted {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "各把密钥的额度之和不能超过你的总额度",
                "granted_micro_usd": granted,
                "other_keys_micro_usd": others,
                "available_micro_usd": granted - others,
            })),
        )
            .into_response();
    }

    let r = if req.micro_usd == 0 {
        st.store.drop_key_quota(&uid, &full_name).await
    } else {
        st.store
            .set_key_quota(&uid, &full_name, req.micro_usd)
            .await
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

fn write_failed(e: WriteError) -> Response {
    let code = match e {
        WriteError::Contended => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (code, Json(json!({"error": e.to_string()}))).into_response()
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
