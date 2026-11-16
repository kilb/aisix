//! 批量计价仍走真实遥测入口，并直接统计数据库实际执行的 charge INSERT。

mod support;

use serde_json::json;
use support::world::{deliver, price, seed_model, tokens_680, usage, world};
use uuid::Uuid;

#[tokio::test]
async fn a_batch_records_all_its_charges_in_one_statement() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    // statement trigger 记录实际新行，单行循环会留下 N 条而非 1 条。
    for sql in [
        "CREATE TABLE test_charge_statements (events text[] NOT NULL)",
        "CREATE FUNCTION test_count_charge_statements() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN INSERT INTO test_charge_statements \
         SELECT COALESCE(array_agg(event_id ORDER BY event_id COLLATE \"C\"), ARRAY[]::text[]) \
         FROM new_charges; RETURN NULL; END $$",
        "CREATE TRIGGER test_charge_statements AFTER INSERT ON pricing_charges_cny \
         REFERENCING NEW TABLE AS new_charges FOR EACH STATEMENT \
         EXECUTE FUNCTION test_count_charge_statements()",
    ] {
        sqlx::query(sql)
            .execute(w.cp.db().pool())
            .await
            .expect("install the statement observer");
    }
    let mut expected_statements = Vec::new();
    let mut previous_statements = 0;
    for count in [1, 10, 100] {
        let ids: Vec<_> = (0..count)
            .map(|i| format!("batch-{count}-{i:03}"))
            .collect();
        // 倒序输入使顺序断言能识别直接照客户端顺序写入的回归。
        let events = ids
            .iter()
            .rev()
            .map(|id| usage(&w, id, model, tokens_680()))
            .collect();
        let started = std::time::Instant::now();
        let response = deliver(&w.dp, Uuid::new_v4(), events).await;
        let elapsed = started.elapsed();
        assert_eq!(response.status, 200, "{response:?}");
        assert_eq!(
            response.body,
            json!({"accepted": count, "rejected": 0, "duplicates": 0})
        );
        expected_statements.push(ids.clone());
        let statements: Vec<Vec<String>> =
            sqlx::query_scalar("SELECT events FROM test_charge_statements ORDER BY events[1]")
                .fetch_all(w.cp.db().pool())
                .await
                .expect("read the statement observer");
        eprintln!(
            "charge batch N={count}: statements={}, elapsed_us={}",
            statements.len() - previous_statements,
            elapsed.as_micros()
        );
        previous_statements = statements.len();
        let amounts: Vec<(String, i64)> = sqlx::query_as(
            "SELECT event_id, charge_micro_cny FROM pricing_charges_cny \
             WHERE event_id = ANY($1) ORDER BY seq",
        )
        .bind(&ids)
        .fetch_all(w.cp.db().pool())
        .await
        .expect("read the charged amounts");
        assert_eq!(
            amounts,
            ids.into_iter().map(|id| (id, 680)).collect::<Vec<_>>()
        );
    }
    let statements: Vec<Vec<String>> =
        sqlx::query_scalar("SELECT events FROM test_charge_statements ORDER BY events[1]")
            .fetch_all(w.cp.db().pool())
            .await
            .expect("read every observed charge statement");
    expected_statements.sort();
    assert_eq!(
        statements, expected_statements,
        "each batch must issue exactly one charge statement"
    );
    let entries: Vec<(String, i64)> = sqlx::query_as(
        "SELECT event_id, amount_micro_cny FROM billing_ledger_cny \
         WHERE kind = 'usage' ORDER BY event_id COLLATE \"C\"",
    )
    .fetch_all(w.cp.db().pool())
    .await
    .expect("read the debits");
    let mut expected_debits = expected_statements
        .into_iter()
        .flatten()
        .map(|id| (id, -680))
        .collect::<Vec<_>>();
    expected_debits.sort();
    assert_eq!(entries, expected_debits);
}

#[tokio::test]
async fn a_batch_keeps_the_first_charge_and_reports_each_conflict() {
    use aisix_control_plane::pricing::store::{
        record_charges, ChargeError, ChargeIdentity, Recorded,
    };
    use chrono::Utc;

    let Some(w) = world().await else { return };
    let now = Utc::now();
    let mut conn =
        w.cp.db()
            .pool()
            .acquire()
            .await
            .expect("a database connection");
    for event_id in ["conflict-a", "conflict-z"] {
        sqlx::query(
            "INSERT INTO usage_events (event_id, dp_id, batch_id, occurred_at, event) \
                     VALUES ($1, $2, $3, $4, '{}')",
        )
        .bind(event_id)
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .bind(now)
        .execute(&mut *conn)
        .await
        .expect("store the usage row");
    }
    let errors = [
        Err(ChargeError::Undecodable),
        Err(ChargeError::TenantMismatch),
        Err(ChargeError::UnknownApiKey),
    ];
    let records: Vec<_> = ["conflict-a", "conflict-a", "conflict-z"]
        .into_iter()
        .zip(&errors)
        .map(|(event_id, charge)| {
            (
                ChargeIdentity {
                    event_id,
                    occurred_at: now,
                    team_id: "",
                    api_key_id: "",
                    user_id: "",
                    requested_model_id: "",
                    group_id: None,
                    request_id: None,
                },
                charge,
            )
        })
        .collect();
    assert_eq!(
        record_charges(&mut conn, &records)
            .await
            .expect("record the batch"),
        vec![
            Recorded::Inserted,
            Recorded::AlreadyRecorded,
            Recorded::Inserted
        ]
    );
    assert_eq!(
        record_charges(&mut conn, &records)
            .await
            .expect("repeat the batch"),
        vec![Recorded::AlreadyRecorded; 3]
    );
    let stored: Vec<(String, String)> =
        sqlx::query_as("SELECT event_id, error_code FROM pricing_charges_cny ORDER BY seq")
            .fetch_all(&mut *conn)
            .await
            .expect("read the stored charge errors");
    assert_eq!(
        stored,
        vec![
            ("conflict-a".to_string(), "undecodable_usage".to_string()),
            ("conflict-z".to_string(), "unknown_api_key".to_string()),
        ]
    );
    assert!(record_charges(&mut conn, &[])
        .await
        .expect("an empty batch")
        .is_empty());
    assert_eq!(w.cp.count("billing_ledger_cny").await, 0);
}
