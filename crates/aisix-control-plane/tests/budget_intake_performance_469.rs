//! 真实 PostgreSQL 的额度计费与投影基准；耗时仅记录，正确性用金额、流水与资源断言。

mod support;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use aisix_control_plane::billing::budgets::{self, ChargedRow, CountMode};
use aisix_control_plane::billing::{ledger, marks};
use aisix_control_plane::resources::write::WriteCtx;
use aisix_control_plane::tenants;
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{json, Value};
use sqlx::PgConnection;
use support::world::{world, World};
use tracing::field::{Field, Visit};
use tracing::instrument::WithSubscriber;
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;
use uuid::Uuid;

const OPENING_BALANCE: i64 = 100_000_000;
const ROW_CHARGE: i64 = 680;
const QUOTA_LIMIT: i64 = 50_000;

#[derive(Clone, Default)]
struct Queries(Arc<Mutex<Vec<(String, u64)>>>);

#[derive(Default)]
struct QueryFields {
    statement: String,
    returned: u64,
}

impl Visit for QueryFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "db.statement" {
            self.statement = value.to_owned();
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "rows_returned" {
            self.returned = value;
        }
    }

    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

impl<S: Subscriber> Layer<S> for Queries {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        if event.metadata().target() == "sqlx::query" {
            let mut fields = QueryFields::default();
            event.record(&mut fields);
            self.0
                .lock()
                .expect("query records")
                .push((fields.statement, fields.returned));
        }
    }
}

impl Queries {
    fn matching(&self, table: &str) -> Vec<(String, u64)> {
        self.0
            .lock()
            .expect("query records")
            .iter()
            .filter(|(sql, _)| sql.trim_start().starts_with("SELECT") && sql.contains(table))
            .cloned()
            .collect()
    }
}

/// 批量准备真实密钥与合法平台额度；只在该测试的独立数据库、未提交事务中使用。
async fn prepare(
    conn: &mut PgConnection,
    w: &World,
    quotas: usize,
    batch: usize,
) -> (Vec<Uuid>, Vec<ChargedRow>, Vec<String>, DateTime<Utc>) {
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *conn)
        .await
        .expect("database clock");
    let key = Uuid::parse_str(&w.key).expect("seed key UUID");
    let ids: Vec<Uuid> = (0..quotas).map(|_| Uuid::new_v4()).collect();
    let refs: Vec<Uuid> = (0..quotas)
        .map(|index| if index == 0 { key } else { Uuid::new_v4() })
        .collect();
    sqlx::query(
        "INSERT INTO tenant_keys \
         (id, tenant_id, member_id, label, key_hash, key_prefix, model_ids, group_id, \
          doc_version, created_by_kind) \
         SELECT n.id, k.tenant_id, k.member_id, 'quota-perf-469', \
                replace(n.id::text, '-', '') || replace(n.id::text, '-', ''), 'sk-perf469', \
                k.model_ids, k.group_id, k.doc_version, 'system' \
         FROM unnest($1::uuid[]) AS n(id) \
         CROSS JOIN tenant_keys k WHERE k.id = $2 AND n.id <> $2",
    )
    .bind(&refs)
    .bind(key)
    .execute(&mut *conn)
    .await
    .expect("prepare real target keys");
    sqlx::query(
        "INSERT INTO billing_budgets_cny \
         (id, tenant_id, scope, scope_ref, set_by, kind, counted_from, limit_micro_cny, \
          created_by_kind) \
         SELECT id, $3, 'api_key', target, 'operator', 'cumulative', $4, $5, 'system' \
         FROM unnest($1::uuid[], $2::uuid[]) AS q(id, target)",
    )
    .bind(&ids)
    .bind(&refs)
    .bind(w.tenant)
    .bind(now - TimeDelta::days(1))
    .bind(QUOTA_LIMIT)
    .execute(&mut *conn)
    .await
    .expect("prepare legal quotas");
    sqlx::query(
        "UPDATE billing_accounts SET balance_micro_cny = $2, marks_dirty = false \
         WHERE tenant_id = $1",
    )
    .bind(w.tenant)
    .bind(OPENING_BALANCE)
    .execute(&mut *conn)
    .await
    .expect("opening balance fixture");
    let prefix = format!("quota-perf-{}", Uuid::new_v4());
    let events: Vec<String> = (0..batch).map(|n| format!("{prefix}-{n}")).collect();
    sqlx::query(
        "INSERT INTO usage_events \
         (event_id, dp_id, batch_id, occurred_at, team_id, api_key_id, event) \
         SELECT id, $2, $2, $3, $4::text, $5::text, \
                jsonb_build_object('event_id', id, 'team_id', $4::text, 'api_key_id', $5::text) \
         FROM unnest($1::text[]) AS e(id)",
    )
    .bind(&events)
    .bind(Uuid::new_v4())
    .bind(now)
    .bind(w.tenant)
    .bind(key)
    .execute(&mut *conn)
    .await
    .expect("prepare real usage rows");
    sqlx::query(
        "INSERT INTO pricing_charges_cny \
         (event_id, tenant_id, api_key_id, user_id, requested_model_id, occurred_at, \
          price_instant, status, charge_micro_cny, engine_version) \
         SELECT id, $2::text, $3::text, $4::text, 'quota-perf-469', $5, $5, 'charged', $6, 1 \
         FROM unnest($1::text[]) AS e(id)",
    )
    .bind(&events)
    .bind(w.tenant)
    .bind(key)
    .bind(w.member)
    .bind(now)
    .bind(ROW_CHARGE)
    .execute(&mut *conn)
    .await
    .expect("prepare charged rows referenced by ledger");
    let rows = (0..batch)
        .map(|_| ChargedRow {
            tenant_id: w.tenant,
            member_id: Some(w.member),
            api_key_id: key,
            price_instant: now,
            amount: ROW_CHARGE,
        })
        .collect();
    (ids, rows, events, now)
}

async fn project(conn: &mut PgConnection, w: &World) {
    let (tenant, _) = tenants::lock_billing(&mut *conn, w.tenant)
        .await
        .expect("tenant account before resources");
    marks::project(conn, &WriteCtx::of(w.cp.state()), &tenant)
        .await
        .expect("project balance and quota resources");
}

/// Q×B 基准计时包含真实账户锁、额度计数、逐笔流水/余额更新与完整额度投影。
/// 准备与验证不计时；每轮回滚，因此输入、金额与资源状态一致且不污染别的测试。
#[tokio::test]
async fn postgres_intake_and_settlement_matrix_preserves_every_amount() {
    let Some(w) = world().await else { return };
    for quotas in [0, 1, 16, 128, 1024] {
        for batch in [100, 1000] {
            for repetition in 0..3 {
                let mut tx =
                    w.cp.db()
                        .pool()
                        .begin()
                        .await
                        .expect("begin isolated sample");
                let (ids, rows, events, _) = prepare(&mut tx, &w, quotas, batch).await;
                project(&mut tx, &w).await;
                let initial: Vec<(Uuid, Value, i64)> = sqlx::query_as(
                    "SELECT id, doc, seq FROM gw_resources \
                     WHERE kind = 'budgets' AND id = ANY($1) ORDER BY id",
                )
                .bind(&ids)
                .fetch_all(&mut *tx)
                .await
                .expect("initial quota projection");
                assert_eq!(initial.len(), quotas);
                let postings: Vec<_> = events
                    .iter()
                    .map(|event| ledger::Posting {
                        tenant_id: w.tenant,
                        amount_micro_cny: -ROW_CHARGE,
                        source: ledger::Source::Usage { event_id: event },
                    })
                    .collect();
                let started = Instant::now();
                ledger::lock_account(&mut tx, w.tenant)
                    .await
                    .expect("intake account lock");
                let flag = budgets::count_charges(&mut tx, &rows, CountMode::Usage)
                    .await
                    .expect("count the actual batch");
                let ledger = ledger::post_flagging(&mut tx, &postings, &flag)
                    .await
                    .expect("post every charged event and account flag");
                let intake_ms = started.elapsed().as_secs_f64() * 1000.0;
                let settlement = Instant::now();
                project(&mut tx, &w).await;
                let settle_ms = settlement.elapsed().as_secs_f64() * 1000.0;
                let total_ms = started.elapsed().as_secs_f64() * 1000.0;
                let amount = ROW_CHARGE * batch as i64;
                assert_eq!(flag, if quotas == 0 { vec![] } else { vec![w.tenant] });
                assert_eq!(ledger.len(), batch);
                for (index, entry) in ledger.iter().enumerate() {
                    assert_eq!(entry.event_id.as_deref(), Some(events[index].as_str()));
                    assert_eq!(entry.amount_micro_cny, -ROW_CHARGE);
                    assert_eq!(
                        entry.balance_after_micro_cny,
                        OPENING_BALANCE - ROW_CHARGE * (index as i64 + 1)
                    );
                }
                let balance: (i64, bool) = sqlx::query_as(
                    "SELECT balance_micro_cny, marks_dirty FROM billing_accounts WHERE tenant_id = $1",
                )
                .bind(w.tenant)
                .fetch_one(&mut *tx)
                .await
                .expect("exact final account");
                assert_eq!(balance, (OPENING_BALANCE - amount, false));
                let spent: Vec<(Uuid, i64)> = sqlx::query_as(
                    "SELECT budget_id, spent_micro_cny FROM billing_budget_spend_cny \
                     WHERE budget_id = ANY($1) ORDER BY budget_id",
                )
                .bind(&ids)
                .fetch_all(&mut *tx)
                .await
                .expect("exact quota spend");
                assert_eq!(
                    spent,
                    if quotas == 0 {
                        vec![]
                    } else {
                        vec![(ids[0], amount)]
                    }
                );
                let final_docs: Vec<(Uuid, Value, i64)> = sqlx::query_as(
                    "SELECT id, doc, seq FROM gw_resources \
                     WHERE kind = 'budgets' AND id = ANY($1) ORDER BY id",
                )
                .bind(&ids)
                .fetch_all(&mut *tx)
                .await
                .expect("settled quota documents");
                assert_eq!(final_docs.len(), quotas, "every quota retains its document");
                for ((id, doc, seq), (old_id, old_doc, old_seq)) in final_docs.iter().zip(&initial)
                {
                    assert_eq!(id, old_id);
                    if quotas > 0 && *id == ids[0] {
                        assert!(doc.get("exhausted").is_some(), "quota crossed: {doc}");
                        assert!(*seq > *old_seq);
                    } else {
                        assert_eq!((doc, seq), (old_doc, old_seq), "unrelated quota unchanged");
                    }
                }
                let semantic = json!({"quotas": quotas, "batch": batch,
                    "charged": amount, "balance": balance.0, "ledger_rows": ledger.len(),
                    "counted_quotas": spent.len(), "exhausted_quotas": usize::from(quotas > 0)});
                let hash = ring::digest::digest(
                    &ring::digest::SHA256,
                    serde_json::to_string(&semantic)
                        .expect("semantic JSON")
                        .as_bytes(),
                );
                eprintln!(
                    "PERF469 {}",
                    json!({"quotas": quotas, "batch": batch,
                    "repetition": repetition, "intake_ms": intake_ms, "settle_ms": settle_ms,
                    "total_ms": total_ms, "semantic_sha256": hex::encode(hash)})
                );
                tx.rollback().await.expect("discard the entire sample");
            }
        }
    }
}

/// 回归判断来自真实 sqlx 查询事件：默认查询至少一次，避免关闭日志产生假绿。
#[tokio::test]
async fn intake_fetches_only_applicable_quotas() {
    let Some(w) = world().await else { return };
    let mut tx =
        w.cp.db()
            .pool()
            .begin()
            .await
            .expect("begin isolated census");
    let (_, rows, _, _) = prepare(&mut tx, &w, 128, 100).await;
    project(&mut tx, &w).await;
    let intake_queries = Queries::default();
    let subscriber = tracing_subscriber::registry().with(intake_queries.clone());
    budgets::count_charges(&mut tx, &rows, CountMode::Usage)
        .with_subscriber(subscriber)
        .await
        .expect("count with real query instrumentation");
    let quota_reads = intake_queries.matching("FROM billing_budgets_cny");
    assert_eq!(
        quota_reads.len(),
        1,
        "one real quota query: {quota_reads:?}"
    );
    assert_eq!(
        quota_reads[0].1, 1,
        "only the batch key's applicable quota is fetched"
    );
    eprintln!(
        "PERF469_QUERY {}",
        json!({"quota_selects": quota_reads.len(),
        "quota_rows_returned": quota_reads[0].1})
    );
    tx.rollback().await.expect("discard intake census");
}

#[tokio::test]
async fn stable_projection_reads_resources_in_bulk() {
    let Some(w) = world().await else { return };
    let mut tx =
        w.cp.db()
            .pool()
            .begin()
            .await
            .expect("begin isolated census");
    prepare(&mut tx, &w, 128, 100).await;
    project(&mut tx, &w).await;
    let projection_queries = Queries::default();
    let subscriber = tracing_subscriber::registry().with(projection_queries.clone());
    project(&mut tx, &w).with_subscriber(subscriber).await;
    let resource_reads = projection_queries.matching("FROM gw_resources");
    assert!(
        !resource_reads.is_empty(),
        "real resource reads must be observable"
    );
    assert!(
        resource_reads.len() <= 3,
        "stable balance and 128 quota docs should use constant resource reads, got {}",
        resource_reads.len()
    );
    eprintln!(
        "PERF469_QUERY {}",
        json!({"resource_selects": resource_reads.len()})
    );
    tx.rollback().await.expect("discard census");
}

/// 直接批量计数仍须同时匹配租户、层级及对象；历史周期和累计调整各按自己的范围记账。
#[tokio::test]
async fn matching_keeps_tenants_scopes_setters_periods_and_adjustments_distinct() {
    use aisix_control_plane::auth::MemberRole;
    use support::world::{seed_key, seed_tenant};

    let Some(w) = world().await else { return };
    let other_tenant = seed_tenant(&w.cp, &w.admin).await;
    let other_member = w.cp.seed_member(other_tenant, MemberRole::Member).await.id;
    let other_key =
        Uuid::parse_str(&seed_key(&w.cp, &w.admin, other_tenant, Some(other_member)).await)
            .expect("other tenant key");
    let key1 = Uuid::parse_str(&w.key).expect("first key");
    let key2 = Uuid::parse_str(&seed_key(&w.cp, &w.admin, w.tenant, Some(w.member)).await)
        .expect("same member second key");
    let mut tx =
        w.cp.db()
            .pool()
            .begin()
            .await
            .expect("begin exact matching case");
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *tx)
        .await
        .expect("clock");
    // 不同表允许同一个UUID；成员额度不能误算成同UUID密钥的额度。
    sqlx::query(
        "INSERT INTO tenant_keys \
         (id, tenant_id, member_id, label, key_hash, key_prefix, model_ids, group_id, \
          doc_version, created_by_kind) \
         SELECT $1, k.tenant_id, NULL, 'scope-collision-469', \
                replace($1::text, '-', '') || replace($1::text, '-', ''), 'sk-scope469', \
                k.model_ids, k.group_id, k.doc_version, 'system' \
         FROM tenant_keys k WHERE k.id = $2",
    )
    .bind(w.member)
    .bind(key1)
    .execute(&mut *tx)
    .await
    .expect("actual same-UUID key");
    let specs: Vec<(Uuid, &str, Uuid, &str, Option<&str>, bool)> = vec![
        (w.tenant, "team", w.tenant, "operator", None, false),
        (w.tenant, "member", w.member, "org_admin", None, false),
        (w.tenant, "api_key", key1, "operator", None, false),
        (w.tenant, "api_key", key1, "org_admin", None, false),
        (w.tenant, "api_key", key1, "member", None, false),
        (w.tenant, "api_key", key2, "operator", None, false),
        (w.tenant, "api_key", w.member, "operator", None, false),
        (
            w.tenant,
            "member",
            w.member,
            "org_admin",
            Some("day"),
            false,
        ),
        (w.tenant, "team", w.tenant, "operator", Some("month"), false),
        (
            w.tenant,
            "member",
            w.member,
            "org_admin",
            Some("week"),
            false,
        ),
        (w.tenant, "api_key", key1, "operator", Some("day"), true),
        (other_tenant, "team", other_tenant, "operator", None, false),
        (
            other_tenant,
            "member",
            other_member,
            "org_admin",
            None,
            false,
        ),
    ];
    let ids: Vec<Uuid> = (0..specs.len()).map(|_| Uuid::new_v4()).collect();
    for (id, (tenant, scope, target, setter, period, deleted)) in ids.iter().zip(&specs) {
        sqlx::query(
            "INSERT INTO billing_budgets_cny \
             (id, tenant_id, scope, scope_ref, set_by, kind, period, counted_from, \
              limit_micro_cny, created_by_kind, deleted_at) \
             VALUES ($1,$2,$3,$4,$5,CASE WHEN $6::text IS NULL THEN 'cumulative' \
                     ELSE 'periodic' END,$6,CASE WHEN $6::text IS NULL THEN $7::timestamptz \
                     ELSE NULL END,1000,'system',CASE WHEN $8 THEN now() ELSE NULL END)",
        )
        .bind(id)
        .bind(tenant)
        .bind(scope)
        .bind(target)
        .bind(setter)
        .bind(period)
        .bind(now - TimeDelta::days(2))
        .bind(deleted)
        .execute(&mut *tx)
        .await
        .expect("legal scope/setter/period fixture");
    }
    let mut tenants_to_lock = vec![w.tenant, other_tenant];
    let locked = tenants::lock_many_billing(&mut tx, &mut tenants_to_lock)
        .await
        .expect("all billing locks before any resource write");
    let ctx = WriteCtx::of(w.cp.state());
    for (tenant, _) in &locked {
        marks::project(&mut tx, &ctx, tenant)
            .await
            .expect("initial marks");
    }
    let make = |tenant_id, member_id, api_key_id, price_instant, amount| ChargedRow {
        tenant_id,
        member_id: Some(member_id),
        api_key_id,
        price_instant,
        amount,
    };
    let rows = vec![
        make(w.tenant, w.member, key1, now, 1200),
        make(w.tenant, w.member, key2, now, 600),
        make(w.tenant, w.member, key1, now - TimeDelta::days(40), 800),
        make(other_tenant, other_member, other_key, now, 900),
    ];
    let crossed = budgets::count_charges(&mut tx, &rows, CountMode::Usage)
        .await
        .expect("mixed tenant exact count");
    assert_eq!(crossed, vec![w.tenant]);
    let current_expected = [
        1800, 1800, 1200, 1200, 1200, 600, 0, 1800, 1800, 1800, 0, 900, 900,
    ];
    for (index, id) in ids.iter().enumerate() {
        let entries: Vec<(DateTime<Utc>, i64)> = sqlx::query_as(
            "SELECT period_start, spent_micro_cny FROM billing_budget_spend_cny \
             WHERE budget_id = $1 ORDER BY period_start",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await
        .expect("all exact spend buckets");
        let expected: Vec<i64> = if index == 6 || index == 10 {
            vec![]
        } else if (7..=9).contains(&index) {
            vec![800, 1800]
        } else {
            vec![current_expected[index]]
        };
        assert_eq!(
            entries.iter().map(|(_, value)| *value).collect::<Vec<_>>(),
            expected,
            "quota {index} must count exactly its own subjects and periods"
        );
    }
    for (tenant, _) in &locked {
        marks::project(&mut tx, &ctx, tenant)
            .await
            .expect("exhausted marks");
    }
    let before: (Value, i64) =
        sqlx::query_as("SELECT doc, seq FROM gw_resources WHERE kind='budgets' AND id=$1")
            .bind(ids[0])
            .fetch_one(&mut *tx)
            .await
            .expect("existing exhausted doc");
    assert!(before.0.get("exhausted").is_some());
    sqlx::query("UPDATE billing_budgets_cny SET marked_exhausted=false WHERE id=$1")
        .bind(ids[0])
        .execute(&mut *tx)
        .await
        .expect("stale metadata fixture");
    // 一个资源缺失，一个留下墓碑；两者都应被同一结算恢复。
    sqlx::query("DELETE FROM gw_resources WHERE kind='budgets' AND id=$1")
        .bind(ids[3])
        .execute(&mut *tx)
        .await
        .expect("missing projection fixture");
    sqlx::query("UPDATE gw_resources SET deleted=true WHERE kind='budgets' AND id=$1")
        .bind(ids[4])
        .execute(&mut *tx)
        .await
        .expect("tombstone fixture");
    for (tenant, _) in &locked {
        marks::project(&mut tx, &ctx, tenant)
            .await
            .expect("restore projection and metadata");
    }
    let after: (Value, i64) =
        sqlx::query_as("SELECT doc, seq FROM gw_resources WHERE kind='budgets' AND id=$1")
            .bind(ids[0])
            .fetch_one(&mut *tx)
            .await
            .expect("same doc after metadata repair");
    assert_eq!(
        after, before,
        "metadata-only repair must not rewrite equal document"
    );
    let marked: Option<bool> =
        sqlx::query_scalar("SELECT marked_exhausted FROM billing_budgets_cny WHERE id=$1")
            .bind(ids[0])
            .fetch_one(&mut *tx)
            .await
            .expect("repaired metadata");
    assert_eq!(marked, Some(true));
    for id in [ids[3], ids[4]] {
        let restored: (Value, bool) =
            sqlx::query_as("SELECT doc, deleted FROM gw_resources WHERE kind='budgets' AND id=$1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await
                .expect("restored live resource");
        assert!(!restored.1);
        assert!(restored.0.get("exhausted").is_some());
    }
    let adjusted = budgets::count_charges(
        &mut tx,
        &[
            make(w.tenant, w.member, key1, now, -400),
            make(w.tenant, w.member, key2, now, -100),
            make(other_tenant, other_member, other_key, now, -50),
        ],
        CountMode::Adjustment,
    )
    .await
    .expect("negative cumulative adjustments");
    assert_eq!(
        adjusted,
        vec![w.tenant],
        "three key setters each lift their exhausted mark"
    );
    for (tenant, _) in &locked {
        marks::project(&mut tx, &ctx, tenant)
            .await
            .expect("lift exhausted key marks");
    }
    let adjusted_expected = [
        1300, 1300, 800, 800, 800, 500, 0, 1800, 1800, 1800, 0, 850, 850,
    ];
    for (index, id) in ids.iter().enumerate() {
        let entries: Vec<i64> = sqlx::query_scalar(
            "SELECT spent_micro_cny FROM billing_budget_spend_cny \
             WHERE budget_id=$1 ORDER BY period_start",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await
        .expect("adjusted exact buckets");
        let expected = if index == 6 || index == 10 {
            vec![]
        } else if (7..=9).contains(&index) {
            vec![800, 1800]
        } else {
            vec![adjusted_expected[index]]
        };
        assert_eq!(
            entries, expected,
            "quota {index}: adjustments must leave periods alone"
        );
        if index != 10 {
            let doc: Value = sqlx::query_scalar(
                "SELECT doc FROM gw_resources WHERE kind='budgets' AND id=$1 AND NOT deleted",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .expect("every live quota remains visible");
            assert_eq!(
                doc.get("exhausted").is_some(),
                adjusted_expected[index] >= 1000,
                "quota {index}: final mark follows exact own spend"
            );
        }
    }
    tx.rollback().await.expect("discard semantic case");
}

#[tokio::test]
async fn unconfigured_intake_keeps_one_empty_quota_query() {
    let Some(w) = world().await else { return };
    let mut tx =
        w.cp.db()
            .pool()
            .begin()
            .await
            .expect("begin default-path census");
    let (_, rows, _, _) = prepare(&mut tx, &w, 0, 100).await;
    let captured = Queries::default();
    let subscriber = tracing_subscriber::registry().with(captured.clone());
    let flagged = budgets::count_charges(&mut tx, &rows, CountMode::Usage)
        .with_subscriber(subscriber)
        .await
        .expect("unconfigured count");
    assert!(flagged.is_empty());
    let reads = captured.matching("FROM billing_budgets_cny");
    assert_eq!(
        reads.len(),
        1,
        "existing empty-quota path stays one query: {reads:?}"
    );
    assert_eq!(reads[0].1, 0, "no unrelated quota rows decoded");
    eprintln!(
        "PERF469_QUERY_DEFAULT {}",
        json!({"quota_selects":reads.len(),"quota_rows_returned":reads[0].1})
    );
    tx.rollback().await.expect("discard default-path census");
}
