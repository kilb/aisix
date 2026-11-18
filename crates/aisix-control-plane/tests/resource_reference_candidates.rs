//! 真实 Postgres 检查候选收窄：无关文档不能被读取，所有已有引用仍被精确识别。

mod support;

use aisix_control_plane::resources::refs::{incoming, Referrer};
use aisix_control_plane::resources::Kind;
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

async fn connection() -> Option<PgConnection> {
    let url = support::gate_var("CP_TEST_DATABASE_URL")?;
    let mut conn = PgConnection::connect(&url)
        .await
        .expect("连接测试 Postgres");
    sqlx::raw_sql(
        "CREATE TEMP TABLE gw_resources (kind text NOT NULL, id uuid NOT NULL, \
         doc jsonb NOT NULL, deleted boolean NOT NULL DEFAULT false, managed_by text)",
    )
    .execute(&mut conn)
    .await
    .expect("创建连接私有资源表");
    Some(conn)
}

async fn insert(conn: &mut PgConnection, kind: Kind, doc: Value, owner: Option<&str>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO gw_resources(kind,id,doc,managed_by) VALUES ($1,$2,$3,$4)")
        .bind(kind.segment())
        .bind(id)
        .bind(doc)
        .bind(owner)
        .execute(conn)
        .await
        .expect("插入引用场景");
    id
}

#[tokio::test]
async fn a_provider_key_check_does_not_read_unrelated_documents() {
    let Some(mut conn) = connection().await else {
        return;
    };
    let target = Uuid::new_v4();
    let model = insert(
        &mut conn,
        Kind::Models,
        json!({"provider_key_id": target}),
        None,
    )
    .await;
    insert(
        &mut conn,
        Kind::ObservabilityExporters,
        json!({"name": target}),
        None,
    )
    .await;
    // 旧查询读取无关行的 doc 时必定报错，避免用机器负载相关的计时阈值验证剪枝。
    sqlx::raw_sql(
        "ALTER TABLE gw_resources RENAME TO reference_rows; \
         CREATE FUNCTION pg_temp.reference_doc(kind text, doc jsonb) RETURNS jsonb \
         LANGUAGE plpgsql VOLATILE AS $$ BEGIN \
           IF kind = 'observability_exporters' THEN RAISE EXCEPTION '无关文档被读取'; END IF; \
           RETURN doc; END $$; \
         CREATE TEMP VIEW gw_resources AS SELECT kind,id,pg_temp.reference_doc(kind,doc) AS doc, \
         deleted,managed_by FROM reference_rows",
    )
    .execute(&mut conn)
    .await
    .expect("安装读取哨兵");
    let found = incoming(&mut conn, Kind::ProviderKeys, target, &[], &[])
        .await
        .expect("检查不能读取不可能引用上游账号的文档");
    assert_eq!(
        found,
        [Referrer {
            kind: Kind::Models,
            id: model,
            name: None
        }]
    );
}

#[tokio::test]
async fn kinds_without_incoming_references_do_not_query_the_resource_table() {
    let Some(mut conn) = connection().await else {
        return;
    };
    // 表不可见时仍应直接返回；旧实现会以 relation does not exist 失败。
    sqlx::query("ALTER TABLE gw_resources RENAME TO unrelated_resources")
        .execute(&mut conn)
        .await
        .expect("隐藏本连接的资源表");
    sqlx::query("SET search_path = pg_temp")
        .execute(&mut conn)
        .await
        .expect("不允许回落到共享表");
    for kind in [
        Kind::CachePolicies,
        Kind::RateLimitPolicies,
        Kind::GuardrailAttachments,
        Kind::ObservabilityExporters,
        Kind::BudgetPolicy,
        Kind::TokenCounting,
        Kind::Budgets,
    ] {
        assert!(incoming(&mut conn, kind, Uuid::new_v4(), &[], &[])
            .await
            .unwrap_or_else(|e| panic!("{kind:?} 不应查询资源表: {e}"))
            .is_empty());
    }
}

#[tokio::test]
async fn narrowed_candidates_preserve_every_reference_family_and_exclusions() {
    let Some(mut conn) = connection().await else {
        return;
    };
    let target = Uuid::new_v4();
    let cases = [
        (
            Kind::Models,
            Kind::ProviderKeys,
            json!({"provider_key_id": target}),
        ),
        (
            Kind::TokenCounting,
            Kind::ProviderKeys,
            json!({"messages_count_tokens": {"provider_key_id": target}}),
        ),
        (
            Kind::TokenCounting,
            Kind::ProviderKeys,
            json!({"responses_input_tokens": {"provider_key_id": target}}),
        ),
        (
            Kind::Models,
            Kind::Models,
            json!({"routing": {"targets": [{"model_id": target}]}}),
        ),
        (
            Kind::Models,
            Kind::Models,
            json!({"ensemble": {"panel": [{"model_id": target}], "judge": {"model_id": target}}}),
        ),
        (
            Kind::Models,
            Kind::Models,
            json!({"semantic": {"routes": [{"target_id": target}], "embedding_model_id": target, "default_id": target}}),
        ),
        (
            Kind::CachePolicies,
            Kind::Models,
            json!({"applies_to_model_id": target}),
        ),
        (
            Kind::CachePolicies,
            Kind::Models,
            json!({"semantic": {"embedding_model_id": target}}),
        ),
        (
            Kind::Guardrails,
            Kind::Models,
            json!({"embedding_model_id": target}),
        ),
        (
            Kind::ApiKeys,
            Kind::Models,
            json!({"allowed_model_ids": [target]}),
        ),
        (
            Kind::TokenCounting,
            Kind::Models,
            json!({"messages_count_tokens": {"model_mappings": [{"model_id": target}]}}),
        ),
        (
            Kind::TokenCounting,
            Kind::Models,
            json!({"responses_input_tokens": {"model_mappings": [{"model_id": target}]}}),
        ),
        (
            Kind::ProviderKeys,
            Kind::Models,
            json!({"realtime_sip": {"model_id": target}}),
        ),
        (
            Kind::ProviderKeys,
            Kind::ApiKeys,
            json!({"realtime_sip": {"api_key_id": target}}),
        ),
        (
            Kind::GuardrailAttachments,
            Kind::Guardrails,
            json!({"guardrail_id": target}),
        ),
        (
            Kind::GuardrailAttachments,
            Kind::Models,
            json!({"scope_type": "model", "scope_id": target}),
        ),
        (
            Kind::GuardrailAttachments,
            Kind::ApiKeys,
            json!({"scope_type": "api_key", "scope_id": target}),
        ),
        (
            Kind::GuardrailAttachments,
            Kind::Teams,
            json!({"scope_type": "team", "scope_id": target}),
        ),
        (
            Kind::CachePolicies,
            Kind::ApiKeys,
            json!({"applies_to": format!(" api_key: {target} ")}),
        ),
        (
            Kind::RateLimitPolicies,
            Kind::Models,
            json!({"scope": "model", "scope_ref": target}),
        ),
        (
            Kind::RateLimitPolicies,
            Kind::ApiKeys,
            json!({"scope": "api_key", "scope_ref": target}),
        ),
        (
            Kind::RateLimitPolicies,
            Kind::Teams,
            json!({"scope": "team", "scope_ref": target}),
        ),
        (
            Kind::RateLimitPolicies,
            Kind::Teams,
            json!({"scope": "team_member", "scope_ref": target}),
        ),
        (
            Kind::RateLimitPolicies,
            Kind::Users,
            json!({"scope": "member", "scope_ref": target}),
        ),
        (Kind::ApiKeys, Kind::Teams, json!({"team_id": target})),
        (Kind::ApiKeys, Kind::Users, json!({"user_id": target})),
    ];
    let mut expected = Vec::new();
    for (source, target_kind, doc) in cases {
        let id = insert(&mut conn, source, doc, None).await;
        expected.push((source, target_kind, id));
    }
    for target_kind in Kind::ALL {
        let found = incoming(&mut conn, target_kind, target, &[], &[])
            .await
            .expect("查入边");
        let mut actual: Vec<_> = found
            .into_iter()
            .map(|r| (r.kind.segment(), r.id))
            .collect();
        let mut wanted: Vec<_> = expected
            .iter()
            .filter(|(_, k, _)| *k == target_kind)
            .map(|(source, _, id)| (source.segment(), *id))
            .collect();
        actual.sort();
        wanted.sort();
        assert_eq!(actual, wanted, "{target_kind:?}");
    }
    sqlx::query("TRUNCATE gw_resources")
        .execute(&mut conn)
        .await
        .expect("隔离排除规则场景");
    insert(&mut conn, Kind::ApiKeys, json!({"team_id": target}), None).await;
    let derived = insert(
        &mut conn,
        Kind::ApiKeys,
        json!({"team_id": target}),
        Some("tenant:test"),
    )
    .await;
    let deleted = insert(
        &mut conn,
        Kind::ApiKeys,
        json!({"team_id": target}),
        Some("tenant:test"),
    )
    .await;
    sqlx::query("UPDATE gw_resources SET deleted=true WHERE id=$1")
        .bind(deleted)
        .execute(&mut conn)
        .await
        .expect("标记已删除引用");
    insert(&mut conn, Kind::ApiKeys, json!({"name": target}), None).await;
    let found = incoming(&mut conn, Kind::Teams, target, &[], &[Kind::ApiKeys])
        .await
        .expect("排除操作者引用");
    assert_eq!(found.iter().map(|r| r.id).collect::<Vec<_>>(), [derived]);
    assert!(
        incoming(&mut conn, Kind::Teams, target, &[Kind::ApiKeys], &[])
            .await
            .expect("排除指定种类")
            .is_empty()
    );
}
