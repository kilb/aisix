//! 租户管理密码重置：真实会话、权限隔离、原子撤销和秘密不入审计。

mod support;

use std::sync::Arc;
use std::time::Duration;

use aisix_control_plane::auth::MemberRole;
use aisix_control_plane::tenants::TenantStatus;
use serde_json::{json, Value};
use support::members::{MemberSession, SeededMember};
use support::{Resp, TestCp, TEST_PASSWORD};
use uuid::Uuid;

const NEW_PASSWORD: &str = "member-reset-new-password-7c1";

async fn reset(cp: &TestCp, by: &MemberSession, target: Uuid, password: &str) -> Resp {
    cp.send(
        "POST",
        &format!("/api/members/{target}/password"),
        Some(by.cookie()),
        Some(json!({"new_password": password})),
    )
    .await
}

async fn audit_rows(cp: &TestCp, target: Uuid) -> Vec<(String, String, Option<Uuid>, Value)> {
    sqlx::query_as(
        "SELECT action, actor_kind, actor_id, detail FROM audit_log \
         WHERE target_kind = 'member' AND target_id = $1 ORDER BY id",
    )
    .bind(target.to_string())
    .fetch_all(cp.db().pool())
    .await
    .expect("read the target's audit rows")
}

async fn still_uses_old_password(cp: &TestCp, target: &SeededMember) {
    assert_eq!(
        cp.member_login(&target.email, TEST_PASSWORD).await.status,
        200
    );
    assert!(audit_rows(cp, target.id).await.is_empty());
}

#[tokio::test]
async fn a_manager_reset_ends_every_target_session_and_keeps_the_actor_session() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.seed_member(tenant, MemberRole::Owner).await;
    let actor = cp.member_login_as(&owner).await;
    let target = cp.seed_member(tenant, MemberRole::Member).await;
    let first = cp.member_login_as(&target).await;
    let second = cp.member_login_as(&target).await;

    let response = reset(&cp, &actor, target.id, NEW_PASSWORD).await;
    assert_eq!(response.status, 204, "{response:?}");
    for cookie in [first.cookie(), second.cookie()] {
        assert_eq!(cp.get_with_cookie("/api/org", cookie).await.status, 401);
    }
    assert_eq!(
        cp.get_with_cookie("/api/org", actor.cookie()).await.status,
        200
    );
    assert_eq!(
        cp.member_login(&target.email, TEST_PASSWORD).await.status,
        401
    );
    assert_eq!(
        cp.member_login(&target.email, NEW_PASSWORD).await.status,
        200
    );
    let rows = audit_rows(&cp, target.id).await;
    assert_eq!(
        rows,
        [(
            "member.password_reset".to_string(),
            "member".to_string(),
            Some(actor.id),
            json!({})
        )]
    );
    let stored: String = sqlx::query_scalar("SELECT password_hash FROM members WHERE id = $1")
        .bind(target.id)
        .fetch_one(cp.db().pool())
        .await
        .expect("read stored hash");
    assert!(stored.starts_with("$argon2"));
    assert!(!stored.contains(NEW_PASSWORD));
    assert!(!serde_json::to_string(&rows)
        .expect("serialize audit")
        .contains(NEW_PASSWORD));
}

#[tokio::test]
async fn reset_permissions_do_not_cross_tenants_owners_or_the_self_service_boundary() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.seed_member(tenant, MemberRole::Owner).await;
    let admin = cp.seed_member(tenant, MemberRole::Admin).await;
    let member = cp.seed_member(tenant, MemberRole::Member).await;
    let billing = cp.seed_member(tenant, MemberRole::Billing).await;
    let target = cp.seed_member(tenant, MemberRole::Member).await;
    let other_tenant = cp.seed_tenant(TenantStatus::Active).await;
    let outsider = cp.seed_member(other_tenant, MemberRole::Member).await;
    let owner_session = cp.member_login_as(&owner).await;
    let admin_session = cp.member_login_as(&admin).await;
    for actor in [&member, &billing] {
        let session = cp.member_login_as(actor).await;
        let response = reset(&cp, &session, target.id, NEW_PASSWORD).await;
        assert_eq!(response.status, 403, "{response:?}");
    }
    for session in [&owner_session, &admin_session] {
        let response = reset(&cp, session, session.id, NEW_PASSWORD).await;
        assert_eq!(response.status, 403, "{response:?}");
    }
    assert_eq!(
        reset(&cp, &admin_session, owner.id, NEW_PASSWORD)
            .await
            .status,
        403
    );
    for id in [outsider.id, Uuid::new_v4()] {
        let response = reset(&cp, &owner_session, id, NEW_PASSWORD).await;
        assert_eq!(response.status, 404, "{response:?}");
        assert_eq!(response.body["code"], "not_found");
    }
    still_uses_old_password(&cp, &target).await;
    still_uses_old_password(&cp, &owner).await;
    still_uses_old_password(&cp, &admin).await;
    still_uses_old_password(&cp, &outsider).await;
    assert_eq!(
        reset(&cp, &admin_session, target.id, NEW_PASSWORD)
            .await
            .status,
        204
    );
    let other_owner = cp.seed_member(tenant, MemberRole::Owner).await;
    assert_eq!(
        reset(&cp, &owner_session, other_owner.id, NEW_PASSWORD)
            .await
            .status,
        204
    );
}

#[tokio::test]
async fn reset_validates_the_password_without_changing_sessions_and_never_reenables_a_member() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.seed_member(tenant, MemberRole::Owner).await;
    let actor = cp.member_login_as(&owner).await;
    let target = cp.seed_member(tenant, MemberRole::Member).await;
    let before = cp.member_login_as(&target).await;
    let response = reset(&cp, &actor, target.id, "short").await;
    assert_eq!(response.status, 422, "{response:?}");
    assert_eq!(response.body["details"][0]["path"], "/new_password");
    assert_eq!(
        cp.get_with_cookie("/api/org", before.cookie()).await.status,
        200
    );
    still_uses_old_password(&cp, &target).await;
    let response = cp
        .send(
            "POST",
            &format!("/api/members/{}/password", target.id),
            Some(actor.cookie()),
            Some(json!({"new_password": NEW_PASSWORD, "role": "owner"})),
        )
        .await;
    assert_eq!(response.status, 422, "{response:?}");
    still_uses_old_password(&cp, &target).await;

    sqlx::query("UPDATE members SET disabled = true WHERE id = $1")
        .bind(target.id)
        .execute(cp.db().pool())
        .await
        .expect("disable isolated member");
    assert_eq!(
        reset(&cp, &actor, target.id, NEW_PASSWORD).await.status,
        204
    );
    let disabled: bool = sqlx::query_scalar("SELECT disabled FROM members WHERE id = $1")
        .bind(target.id)
        .fetch_one(cp.db().pool())
        .await
        .expect("read status");
    assert!(disabled);
    assert_eq!(
        cp.member_login(&target.email, NEW_PASSWORD).await.status,
        401
    );
}

#[tokio::test]
async fn a_reset_rechecks_authority_and_tenant_state_after_waiting_for_the_tenant_lock() {
    let Some(cp) = TestCp::start_with(|cfg| {
        cfg.db_lock_timeout_ms = 120_000;
    })
    .await
    else {
        return;
    };
    let cp = Arc::new(cp);
    for change in ["caller", "target", "tenant"] {
        let tenant = cp.seed_tenant(TenantStatus::Active).await;
        let admin = cp.seed_member(tenant, MemberRole::Admin).await;
        let actor = cp.member_login_as(&admin).await;
        let target = cp.seed_member(tenant, MemberRole::Member).await;
        let mut holder = cp.db().pool().begin().await.expect("hold tenant");
        sqlx::query("SELECT id FROM tenants WHERE id = $1 FOR UPDATE")
            .bind(tenant)
            .execute(&mut *holder)
            .await
            .expect("lock tenant");
        let reset_task = {
            let cp = cp.clone();
            tokio::spawn(async move { reset(&cp, &actor, target.id, NEW_PASSWORD).await })
        };
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let waiting: i64 = sqlx::query_scalar("SELECT count(*)::bigint FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock'")
                    .fetch_one(cp.db().pool()).await.expect("read lock waiters");
                if waiting > 0 { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("reset waits for the tenant lock");
        match change {
            "caller" => {
                sqlx::query("UPDATE members SET role = 'member' WHERE id = $1")
                    .bind(admin.id)
                    .execute(&mut *holder)
                    .await
                    .expect("demote caller");
            }
            "target" => {
                sqlx::query("UPDATE members SET role = 'owner' WHERE id = $1")
                    .bind(target.id)
                    .execute(&mut *holder)
                    .await
                    .expect("promote target");
            }
            "tenant" => {
                sqlx::query("UPDATE tenants SET status = 'suspended' WHERE id = $1")
                    .bind(tenant)
                    .execute(&mut *holder)
                    .await
                    .expect("suspend tenant");
            }
            _ => unreachable!(),
        }
        holder.commit().await.expect("release tenant");
        let response = reset_task.await.expect("reset finishes");
        assert_eq!(response.status, 403, "{change}: {response:?}");
        still_uses_old_password(&cp, &target).await;
    }
}

#[tokio::test]
async fn inactive_tenants_and_removed_targets_never_accept_a_reset() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    for status in [TenantStatus::Pending, TenantStatus::Suspended] {
        let tenant = cp.seed_tenant(status).await;
        let owner = cp.seed_member(tenant, MemberRole::Owner).await;
        let actor = cp.member_login_as(&owner).await;
        let target = cp.seed_member(tenant, MemberRole::Member).await;
        let response = reset(&cp, &actor, target.id, NEW_PASSWORD).await;
        assert_eq!(response.status, 403, "{response:?}");
        assert_eq!(response.body["code"], "tenant_not_active");
        still_uses_old_password(&cp, &target).await;
    }
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.seed_member(tenant, MemberRole::Owner).await;
    let actor = cp.member_login_as(&owner).await;
    let removed = cp.seed_member(tenant, MemberRole::Member).await;
    let deleted = cp
        .send(
            "DELETE",
            &format!("/api/members/{}", removed.id),
            Some(actor.cookie()),
            None,
        )
        .await;
    assert_eq!(deleted.status, 204, "{deleted:?}");
    let before = audit_rows(&cp, removed.id).await;
    let response = reset(&cp, &actor, removed.id, NEW_PASSWORD).await;
    assert_eq!(response.status, 404, "{response:?}");
    assert_eq!(response.body["code"], "not_found");
    assert_eq!(audit_rows(&cp, removed.id).await, before);
}
