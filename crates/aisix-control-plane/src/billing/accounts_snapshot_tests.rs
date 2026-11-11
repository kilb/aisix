use super::*;

#[path = "../../tests/support/mod.rs"]
mod support;

#[tokio::test]
async fn one_snapshot_keeps_account_and_ledger_before_a_concurrent_credit() {
    let Some(w) = support::world::world().await else {
        return;
    };
    let admin = w.cp.admin_login_seeded(OperatorRole::Admin).await;
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let pause = SnapshotPause {
        reached: reached_tx,
        resume: resume_rx,
    };
    // 先等真实账户 SELECT 完成，再走真实 HTTP 入账并等其提交，最后放行聚合。
    let writer = async {
        let properties = reached_rx
            .await
            .expect("account SELECT reached the barrier");
        let credited =
            w.cp.send(
                "POST",
                &format!("/admin/tenants/{}/credits", w.tenant),
                Some(admin.cookie()),
                Some(json!({"amount_cny":"0.05", "reason":"验证并发快照"})),
            )
            .await;
        assert_eq!(credited.status, 201, "{credited:?}");
        resume_tx.send(()).expect("release the ledger SELECT");
        properties
    };
    let (snapshot, properties) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::join!(balance_snapshot(w.cp.db(), w.tenant, Some(pause)), writer)
    })
    .await
    .expect("snapshot and concurrent posting must finish");
    let f = snapshot.expect("read the old snapshot").0["calculation"].clone();
    // READ COMMITTED 变异会实际读到已提交的 50000，并在这里失败。
    assert_eq!(f["topups_micro_cny"], "0");
    assert_eq!(f["usage_micro_cny"], "0");
    assert_eq!(f["adjustments_micro_cny"], "0");
    assert_eq!(f["balance_micro_cny"], "0");
    assert_eq!(f.get("last_ledger_seq"), Some(&Value::Null));
    assert_eq!(
        properties,
        ("repeatable read".to_string(), "on".to_string())
    );
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let current =
        w.cp.send("GET", "/api/billing/balance", Some(owner.cookie()), None)
            .await;
    assert_eq!(current.status, 200, "{current:?}");
    assert_eq!(current.body["calculation"]["topups_micro_cny"], "50000");
    assert_eq!(current.body["calculation"]["balance_micro_cny"], "50000");
}
