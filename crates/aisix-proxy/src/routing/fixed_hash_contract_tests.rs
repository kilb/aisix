use super::*;
use aisix_core::{ProviderKey, ResourceEntry};
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::Arc;

fn snapshot() -> AisixSnapshot {
    let snapshot = AisixSnapshot::new();
    for (id, name, account) in [
        ("id-a", "alias-a", "account-a"),
        ("id-b", "alias-b", "account-b"),
    ] {
        let model: Model = serde_json::from_value(json!({"display_name":name,
            "provider":"openai","provider_key_id":account,"model_name":"upstream"}))
        .unwrap();
        snapshot.models.insert(ResourceEntry::new(id, model, 1));
        let provider: ProviderKey = serde_json::from_value(json!({"display_name":account,
            "provider":"openai","adapter":"openai","api_key":"test-credential"}))
        .unwrap();
        snapshot
            .provider_keys
            .insert(ResourceEntry::new(account, provider, 1));
    }
    snapshot
}

fn group(failover: bool) -> Arc<Model> {
    Arc::new(
        serde_json::from_value(json!({"display_name":"sessions","routing":{
            "strategy":"consistent_hash","hash_failover":failover,"retries":0,
            "hash_on":[{"type":"header","name":"x-claude-code-session-id"},
                {"type":"header","name":"session-id"}],
            "targets":[{"model_id":"id-a"},{"model_id":"id-b"}]
        }}))
        .unwrap(),
    )
}

fn pick(
    registry: &RoutingRegistry,
    status: &crate::ModelRuntimeStatusTracker,
    snapshot: &AisixSnapshot,
    group: &Arc<Model>,
    headers: &HeaderMap,
    origin: Option<&str>,
) -> Result<Vec<AttemptModel>, ProxyError> {
    resolve_attempt_models(
        registry,
        status,
        &LivePricingIndex::new(),
        snapshot,
        "sessions",
        "group-id",
        group,
        RoutingRequest {
            headers: Some(headers),
            api_key_id: "large-caller-key",
            context_origin: origin,
            ..Default::default()
        },
    )
}

fn headers(session: usize) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("session-id", format!("session-{session}").parse().unwrap());
    headers
}

#[test]
fn fixed_hash_has_one_account_and_recomputes_it_after_restart() {
    // 返回第二个账号、改用随机 seed 或忽略会话头都会破坏此合同。
    let snapshot = snapshot();
    let group = group(false);
    let registry = RoutingRegistry::new();
    let status = crate::ModelRuntimeStatusTracker::new();
    let mut accounts = BTreeSet::new();
    for session in 0..64 {
        let headers = headers(session);
        let before = pick(&registry, &status, &snapshot, &group, &headers, None).unwrap();
        assert_eq!(before.len(), 1);
        let account = before[0].model.provider_key_id.as_deref().unwrap();
        accounts.insert(account.to_string());
        let after = pick(
            &RoutingRegistry::new(),
            &crate::ModelRuntimeStatusTracker::new(),
            &snapshot,
            &group,
            &headers,
            None,
        )
        .unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].model.provider_key_id.as_deref(), Some(account));
    }
    assert_eq!(
        accounts,
        BTreeSet::from(["account-a".to_string(), "account-b".to_string()])
    );
}

#[test]
fn fixed_hash_fails_when_its_account_cools_instead_of_moving() {
    // 先健康过滤再 hash 会把这个会话发往另一个仍然健康的账号。
    let snapshot = snapshot();
    let group = group(false);
    let registry = RoutingRegistry::new();
    let status = crate::ModelRuntimeStatusTracker::new();
    let headers = headers(0);
    let chosen = pick(&registry, &status, &snapshot, &group, &headers, None).unwrap();
    status.mark_cooldown(&chosen[0].id, Duration::from_secs(60), "test");
    let error = pick(&registry, &status, &snapshot, &group, &headers, None)
        .err()
        .unwrap();
    assert_eq!(error.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
}

#[test]
fn fixed_hash_missing_header_cannot_fall_back_to_caller_key() {
    let snapshot = snapshot();
    let error = pick(
        &RoutingRegistry::new(),
        &crate::ModelRuntimeStatusTracker::new(),
        &snapshot,
        &group(false),
        &HeaderMap::new(),
        None,
    )
    .err()
    .unwrap();
    assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
}

#[test]
fn fixed_hash_context_origin_cannot_substitute_another_account() {
    let snapshot = snapshot();
    let group = group(false);
    let registry = RoutingRegistry::new();
    let status = crate::ModelRuntimeStatusTracker::new();
    let headers = headers(0);
    let chosen = pick(&registry, &status, &snapshot, &group, &headers, None).unwrap();
    let other = if chosen[0].id == "id-a" {
        "id-b"
    } else {
        "id-a"
    };
    let with_origin = pick(&registry, &status, &snapshot, &group, &headers, Some(other));
    match with_origin {
        Ok(attempts) => {
            assert_eq!(attempts.len(), 1);
            assert_eq!(
                attempts[0].model.provider_key_id,
                chosen[0].model.provider_key_id
            );
        }
        Err(error) => assert_eq!(error.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE),
    }
}

#[test]
fn existing_hash_still_moves_off_a_cooling_account() {
    let snapshot = snapshot();
    let group = group(true);
    let registry = RoutingRegistry::new();
    let status = crate::ModelRuntimeStatusTracker::new();
    let headers = headers(0);
    let before = pick(&registry, &status, &snapshot, &group, &headers, None).unwrap();
    status.mark_cooldown(&before[0].id, Duration::from_secs(60), "test");
    let after = pick(&registry, &status, &snapshot, &group, &headers, None).unwrap();
    assert_ne!(
        after[0].model.provider_key_id,
        before[0].model.provider_key_id
    );
}

#[test]
fn fixed_hash_missing_resources_cannot_shrink_the_account_pool() {
    // 删除任一已配置引用后都不能拿剩余账号重建更小的环。
    for remove_account in [false, true] {
        let snapshot = snapshot();
        let group = group(false);
        let registry = RoutingRegistry::new();
        let status = crate::ModelRuntimeStatusTracker::new();
        let headers = headers(0);
        let chosen = pick(&registry, &status, &snapshot, &group, &headers, None).unwrap();
        if remove_account {
            snapshot
                .provider_keys
                .remove(chosen[0].model.provider_key_id.as_deref().unwrap());
        } else {
            snapshot.models.remove(&chosen[0].id);
        }
        let error = pick(&registry, &status, &snapshot, &group, &headers, None)
            .err()
            .unwrap();
        assert_eq!(error.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }
}

#[test]
fn fixed_hash_uses_account_identity_across_target_alias_renames() {
    let snapshot = snapshot();
    let group = group(false);
    let registry = RoutingRegistry::new();
    let status = crate::ModelRuntimeStatusTracker::new();
    let before: Vec<_> = (0..32)
        .map(|session| {
            pick(
                &registry,
                &status,
                &snapshot,
                &group,
                &headers(session),
                None,
            )
            .unwrap()[0]
                .model
                .provider_key_id
                .clone()
        })
        .collect();
    for id in ["id-a", "id-b"] {
        let entry = snapshot.models.get_by_id(id).unwrap();
        let mut model = (*entry.value).clone();
        model.display_name = format!("renamed-{id}");
        snapshot.models.insert(ResourceEntry::new(id, model, 2));
    }
    for (session, account) in before.iter().enumerate() {
        assert_eq!(
            &pick(
                &registry,
                &status,
                &snapshot,
                &group,
                &headers(session),
                None
            )
            .unwrap()[0]
                .model
                .provider_key_id,
            account
        );
    }
}

#[test]
fn fixed_hash_cannot_promote_a_backup_when_the_primary_cools() {
    let snapshot = snapshot();
    let mut group = group(false);
    let routing = Arc::make_mut(&mut group).routing.as_mut().unwrap();
    routing.targets[0].priority = Some(10);
    let registry = RoutingRegistry::new();
    let status = crate::ModelRuntimeStatusTracker::new();
    let headers = headers(0);
    let chosen = pick(&registry, &status, &snapshot, &group, &headers, None).unwrap();
    assert_eq!(
        chosen[0].model.provider_key_id.as_deref(),
        Some("account-a")
    );
    status.mark_cooldown("id-a", Duration::from_secs(60), "test");
    assert!(pick(&registry, &status, &snapshot, &group, &headers, None).is_err());
}
