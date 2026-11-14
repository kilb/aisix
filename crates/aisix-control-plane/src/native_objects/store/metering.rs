//! 已观察的单位、覆盖和定价状态分别持久化；缺量不会生成零费用事件。

use aisix_core::native_metering::{
    validate_observed_usage, MeteringTarget, ObservedMeter, ObservedUsage, ReconciliationReason,
    ReconciliationRequest, TypedCompleteRequest,
};

use super::*;

struct TargetContext {
    operation_id: Uuid,
    object: Option<ObjectRow>,
    operation: OperationRow,
    submission: BeginRequest,
}

#[derive(sqlx::FromRow)]
struct MeteringRow {
    object_id: Option<Uuid>,
    target_kind: String,
    state: String,
    terminal_status: Option<String>,
    reason: Option<String>,
    observed_usage: Value,
    artifacts: Value,
    raw_usage: Option<Value>,
    receipt_version: i64,
}

async fn target(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    target: &MeteringTarget,
    allow_async_uncertain: bool,
) -> Result<TargetContext, ApiError> {
    let (operation_id, object, operation) = match target {
        MeteringTarget::Object { lease } => {
            // 已受理的结果不依赖原密钥继续有效，但必须由当前有效租约观察。
            let row = lease_live(&mut *conn, state, dp, lease).await?;
            let op = operation(&mut *conn, state, row.operation_id, false).await?;
            (row.operation_id, Some(row), op)
        }
        MeteringTarget::Operation {
            operation_id,
            api_key_id,
        } => {
            let op = operation(&mut *conn, state, *operation_id, true).await?;
            if op.dp_id != dp.dp_id || op.api_key_id != *api_key_id {
                return Err(ApiError::not_found());
            }
            // 同步推理可能没有可寻址对象；可信终态 receipt 本身证明上游已被观察。
            (*operation_id, None, op)
        }
    };
    let submitted = submission(&operation)?;
    if object.is_none() && submitted.asynchronous {
        let already_bound: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM native_objects WHERE environment_id=$1 AND operation_id=$2)",
        ).bind(state.env.env_id).bind(operation_id).fetch_one(&mut *conn).await.map_err(unavailable)?;
        if !allow_async_uncertain || already_bound || operation.state == "bound" {
            return Err(invalid("/target"));
        }
    }
    let identity = format!("native-metering:{}:{operation_id}", state.env.env_id);
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(identity)
        .execute(&mut *conn)
        .await
        .map_err(unavailable)?;
    Ok(TargetContext {
        operation_id,
        object,
        operation,
        submission: submitted,
    })
}

fn promotes_target(previous: Option<&MeteringRow>, context: &TargetContext) -> bool {
    previous.is_some_and(|row| {
        row.object_id.is_none()
            && row.target_kind == "operation"
            && row.reason.as_deref() == Some("uncertain_submission")
            && row.state != "settled"
    }) && context.submission.asynchronous
        && context.object.is_some()
}

async fn previous(
    conn: &mut PgConnection,
    state: &AppState,
    context: &TargetContext,
) -> Result<Option<MeteringRow>, ApiError> {
    let row: Option<MeteringRow> = sqlx::query_as(
        "SELECT object_id,target_kind,state,terminal_status,reason,observed_usage,artifacts,raw_usage,receipt_version \
         FROM native_metering_records WHERE environment_id=$1 AND operation_id=$2 FOR UPDATE",
    )
    .bind(state.env.env_id)
    .bind(context.operation_id)
    .fetch_optional(conn)
    .await
    .map_err(unavailable)?;
    if let Some(row) = &row {
        let promotion = promotes_target(Some(row), context);
        if !promotion
            && (row.object_id != context.object.as_ref().map(|object| object.id)
                || row.target_kind
                    != if context.object.is_some() {
                        "object"
                    } else {
                        "operation"
                    })
        {
            return Err(conflict());
        }
    } else if context
        .object
        .as_ref()
        .is_some_and(|object| object.completed_at.is_some())
    {
        // 旧结算没有 typed receipt，不能事后用另一个量冒充已结算的依据。
        return Err(conflict());
    }
    Ok(row)
}

fn validate_usage(usage: &[ObservedUsage], required: bool) -> Result<(), ApiError> {
    if usage.len() > MAX_ENTRIES
        || usage.iter().any(|entry| {
            entry.entry_id.len() > 256
                || entry.entry_id.chars().any(char::is_control)
                || !text_ok(&entry.upstream_model, 256)
        })
        || ((required || !usage.is_empty()) && validate_observed_usage(usage).is_err())
    {
        return Err(invalid("/usage"));
    }
    Ok(())
}

fn measurement(usage: &Value) -> Value {
    let mut value = usage.clone();
    if let Some(entries) = value.as_array_mut() {
        entries.sort_by(|left, right| {
            left.get("entry_id")
                .and_then(Value::as_str)
                .cmp(&right.get("entry_id").and_then(Value::as_str))
        });
        for entry in entries {
            if let Some(fields) = entry.as_object_mut() {
                fields.remove("occurred_at");
            }
            if let Some(fields) = entry
                .pointer_mut("/meter/usage")
                .and_then(Value::as_object_mut)
            {
                fields.remove("occurred_at");
            }
        }
    }
    value
}

fn artifacts_union(previous: Option<&MeteringRow>, artifacts: Value) -> Result<Value, ApiError> {
    let mut all = previous
        .and_then(|row| row.artifacts.as_array())
        .cloned()
        .unwrap_or_default();
    for artifact in artifacts.as_array().ok_or_else(|| invalid("/artifacts"))? {
        if !all.contains(artifact) {
            all.push(artifact.clone());
        }
    }
    let value = Value::Array(all);
    if serde_json::to_vec(&value).map_err(unavailable)?.len() > 2 * 1024 * 1024 {
        return Err(invalid("/artifacts"));
    }
    Ok(value)
}

fn replay_state(
    previous: Option<&MeteringRow>,
    terminal_status: Option<&str>,
    usage: &Value,
    artifacts: &Value,
    desired_state: &str,
) -> Result<Option<String>, ApiError> {
    let Some(previous) = previous else {
        return Ok(None);
    };
    if previous
        .terminal_status
        .as_deref()
        .is_some_and(|status| Some(status) != terminal_status)
    {
        return Err(conflict());
    }
    let same_measurement = measurement(&previous.observed_usage) == measurement(usage)
        && previous.artifacts == *artifacts;
    let same = same_measurement && previous.terminal_status.as_deref() == terminal_status;
    if matches!(previous.state.as_str(), "settled" | "outbox_pending") {
        return if same {
            Ok(Some("completed".into()))
        } else {
            Err(conflict())
        };
    }
    if previous.state == "awaiting_reconciliation" {
        if !same_measurement || desired_state == "unknown" {
            return Err(conflict());
        }
        if same && desired_state != "outbox_pending" {
            return Ok(Some("reconciliation".into()));
        }
    } else if same && desired_state == "unknown" {
        return Ok(Some("reconciliation".into()));
    }
    Ok(None)
}

async fn bind_artifacts(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    target: &MeteringTarget,
    artifacts: &[ObservedArtifactEntry],
) -> Result<(), ApiError> {
    if artifacts.len() > MAX_ENTRIES {
        return Err(invalid("/artifacts"));
    }
    if artifacts.is_empty() {
        return Ok(());
    }
    let MeteringTarget::Object { lease } = target else {
        // 同步操作尚无可验证的 artifact 根合同，不接收后悄悄丢弃。
        return Err(invalid("/artifacts"));
    };
    foundation::observed(
        conn,
        state,
        dp,
        BindObservedArtifactsRequest {
            lease: lease.clone(),
            entries: artifacts.to_vec(),
            complete: None,
        },
    )
    .await?;
    Ok(())
}

fn billable_token_usage(
    context: &TargetContext,
    usage: &[ObservedUsage],
    status: &str,
) -> Option<Vec<TerminalUsage>> {
    let object = context.object.as_ref()?;
    let eval = context.submission.kind == NativeObjectKind::EvalRun;
    if object.completed_at.is_some()
        || !matches!(
            context.submission.kind,
            NativeObjectKind::Response | NativeObjectKind::MessageBatch | NativeObjectKind::EvalRun
        )
        || !match context.submission.protocol {
            NativeProtocol::Openai if eval => matches!(status, "completed" | "failed" | "canceled"),
            NativeProtocol::Openai => matches!(
                status,
                "completed" | "failed" | "cancelled" | "incomplete" | "expired"
            ),
            NativeProtocol::Anthropic => status == "ended",
        }
    {
        return None;
    }
    let expected: BTreeSet<&str> = if context.submission.entries.is_empty() {
        BTreeSet::from([""])
    } else {
        context
            .submission
            .entries
            .iter()
            .map(|entry| entry.entry_id.as_str())
            .collect()
    };
    let observed: BTreeSet<&str> = usage.iter().map(|entry| entry.entry_id.as_str()).collect();
    if observed.len() != usage.len()
        || if eval {
            observed.is_empty() || !observed.is_subset(&expected)
        } else {
            observed != expected
        }
    {
        return None;
    }
    usage
        .iter()
        .map(|entry| {
            let expected_model = context
                .submission
                .entries
                .iter()
                .find(|submitted| submitted.entry_id == entry.entry_id)
                .map_or(context.submission.upstream_model.as_str(), |entry| {
                    entry.upstream_model.as_str()
                });
            let ObservedMeter::Tokens { usage } = &entry.meter else {
                return None;
            };
            (entry.upstream_model == expected_model && usage.entry_id == entry.entry_id)
                .then(|| usage.clone())
        })
        .collect()
}

struct ReceiptDraft<'a> {
    desired_state: &'a str,
    terminal_status: Option<&'a str>,
    reason: Option<&'a str>,
    usage: Value,
    artifacts: Value,
    receipt: Value,
    raw_usage: Option<Value>,
}

async fn persist(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    context: &TargetContext,
    previous: Option<&MeteringRow>,
    draft: ReceiptDraft<'_>,
) -> Result<(), ApiError> {
    let ReceiptDraft {
        desired_state,
        terminal_status,
        reason,
        usage,
        artifacts,
        receipt,
        raw_usage,
    } = draft;
    if serde_json::to_vec(&usage).map_err(unavailable)?.len() > 2 * 1024 * 1024
        || serde_json::to_vec(&receipt).map_err(unavailable)?.len() > 4 * 1024 * 1024
        || raw_usage
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(unavailable)?
            .is_some_and(|raw| raw.len() > 1024 * 1024)
    {
        return Err(invalid("/usage"));
    }
    let version = match previous {
        Some(row) => row.receipt_version.checked_add(1).ok_or_else(conflict)?,
        None => 1,
    };
    if previous.is_some() {
        sqlx::query("UPDATE native_metering_records SET state=$3,terminal_status=$4,reason=$5,observed_usage=$6,artifacts=$7,receipt_version=$8,raw_usage=$9,object_id=$10,target_kind=$11,updated_at=clock_timestamp() WHERE environment_id=$1 AND operation_id=$2")
            .bind(state.env.env_id).bind(context.operation_id).bind(desired_state).bind(terminal_status)
            .bind(reason).bind(&usage).bind(&artifacts).bind(version).bind(&raw_usage).bind(context.object.as_ref().map(|object| object.id))
            .bind(if context.object.is_some() { "object" } else { "operation" })
            .execute(&mut *conn).await.map_err(unavailable)?;
    } else {
        sqlx::query("INSERT INTO native_metering_records(environment_id,operation_id,object_id,target_kind,dp_id,tenant_id,api_key_id,state,terminal_status,reason,observed_usage,artifacts,receipt_version,raw_usage) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)")
            .bind(state.env.env_id).bind(context.operation_id).bind(context.object.as_ref().map(|object| object.id))
            .bind(if context.object.is_some() { "object" } else { "operation" }).bind(context.operation.dp_id)
            .bind(context.operation.tenant_id).bind(context.operation.api_key_id).bind(desired_state)
            .bind(terminal_status).bind(reason).bind(&usage).bind(&artifacts).bind(version).bind(&raw_usage)
            .execute(&mut *conn).await.map_err(unavailable)?;
    }
    sqlx::query("INSERT INTO native_metering_receipts(environment_id,operation_id,version,observer_dp_id,state,receipt) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(state.env.env_id).bind(context.operation_id).bind(version).bind(dp.dp_id)
        .bind(desired_state).bind(receipt).execute(&mut *conn).await.map_err(unavailable)?;
    if desired_state != "outbox_pending" {
        if let Some(object) = &context.object {
            if desired_state == "awaiting_reconciliation"
                && reason == Some("unpriced_meter")
                && terminal_status.is_some()
                && usage.as_array().is_some_and(|rows| !rows.is_empty())
            {
                // 上游已结束与账务已结清是两件事；真实量完整时允许清理上游，待对账凭据永久保留。
                sqlx::query("UPDATE native_objects SET pending=false,completed_at=clock_timestamp(),terminal_status=$2,version=version+1 WHERE id=$1 AND completed_at IS NULL")
                    .bind(object.id).bind(terminal_status).execute(&mut *conn).await.map_err(unavailable)?;
            } else {
                // 缺量或观察不完整继续保留可恢复身份，不能把未知量写成零费用完成。
                sqlx::query("UPDATE native_objects SET poll_after=clock_timestamp()+interval '1 minute' WHERE id=$1")
                    .bind(object.id).execute(&mut *conn).await.map_err(unavailable)?;
            }
        }
    }
    Ok(())
}

fn metering_reply(
    context: &TargetContext,
    status: &str,
    metering_state: &str,
    reason: Option<&str>,
) -> NativeReply {
    let mut reply = NativeReply::new(status);
    reply.operation_id = Some(context.operation_id);
    reply.origin = Some(context.submission.origin.clone());
    reply.metering_state = Some(metering_state.to_owned());
    reply.reconciliation_reason = reason.map(str::to_owned);
    reply
}

pub(super) async fn complete_typed(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: TypedCompleteRequest,
) -> Result<NativeReply, ApiError> {
    validate_usage(&body.usage, true)?;
    if !text_ok(&body.terminal_status, 64) {
        return Err(invalid("/terminal_status"));
    }
    let context = target(&mut *conn, state, dp, &body.target, false).await?;
    let previous = previous(&mut *conn, state, &context).await?;
    let usage = serde_json::to_value(&body.usage).map_err(unavailable)?;
    let artifacts = artifacts_union(
        previous.as_ref(),
        serde_json::to_value(&body.artifacts).map_err(unavailable)?,
    )?;
    let token_usage = billable_token_usage(&context, &body.usage, &body.terminal_status);
    let desired = if token_usage.is_some() {
        "outbox_pending"
    } else {
        "awaiting_reconciliation"
    };
    if token_usage.is_none()
        && context
            .object
            .as_ref()
            .is_some_and(|object| object.completed_at.is_none())
        && matches!(
            context.submission.kind,
            NativeObjectKind::Response | NativeObjectKind::MessageBatch | NativeObjectKind::EvalRun
        )
        && body
            .usage
            .iter()
            .all(|entry| matches!(&entry.meter, ObservedMeter::Tokens { .. }))
    {
        return Err(invalid("/usage"));
    }
    if let Some(state) = replay_state(
        previous.as_ref(),
        Some(&body.terminal_status),
        &usage,
        &artifacts,
        desired,
    )? {
        let row = previous.as_ref().ok_or_else(conflict)?;
        if !promotes_target(previous.as_ref(), &context) {
            return Ok(metering_reply(
                &context,
                &state,
                &row.state,
                row.reason.as_deref(),
            ));
        }
    }
    bind_artifacts(&mut *conn, state, dp, &body.target, &body.artifacts).await?;
    if let Some(usage) = token_usage {
        let MeteringTarget::Object { lease } = &body.target else {
            return Err(conflict());
        };
        complete(
            &mut *conn,
            state,
            dp,
            CompleteRequest {
                output_file_ids: Vec::new(),
                lease: lease.clone(),
                terminal_status: body.terminal_status.clone(),
                usage,
            },
        )
        .await?;
    }
    persist(
        conn,
        state,
        dp,
        &context,
        previous.as_ref(),
        ReceiptDraft {
            desired_state: desired,
            terminal_status: Some(&body.terminal_status),
            reason: (desired != "outbox_pending").then_some("unpriced_meter"),
            usage,
            artifacts,
            receipt: serde_json::to_value(&body).map_err(unavailable)?,
            raw_usage: previous.as_ref().and_then(|row| row.raw_usage.clone()),
        },
    )
    .await?;
    Ok(metering_reply(
        &context,
        if desired == "outbox_pending" {
            "completed"
        } else {
            "reconciliation"
        },
        desired,
        (desired != "outbox_pending").then_some("unpriced_meter"),
    ))
}

pub(super) async fn mark_reconciliation(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: ReconciliationRequest,
) -> Result<NativeReply, ApiError> {
    validate_usage(&body.observed_usage, false)?;
    if body
        .terminal_status
        .as_ref()
        .is_some_and(|status| !text_ok(status, 64))
    {
        return Err(invalid("/terminal_status"));
    }
    let context = target(
        &mut *conn,
        state,
        dp,
        &body.target,
        body.reason == ReconciliationReason::UncertainSubmission,
    )
    .await?;
    let previous = previous(&mut *conn, state, &context).await?;
    let usage = serde_json::to_value(&body.observed_usage).map_err(unavailable)?;
    let artifacts = artifacts_union(
        previous.as_ref(),
        serde_json::to_value(&body.artifacts).map_err(unavailable)?,
    )?;
    let desired = if body.observed_usage.is_empty() {
        "unknown"
    } else {
        "awaiting_reconciliation"
    };
    let raw_usage = body
        .raw_usage
        .clone()
        .or_else(|| previous.as_ref().and_then(|row| row.raw_usage.clone()));
    let raw_changed = previous
        .as_ref()
        .is_some_and(|row| row.raw_usage != raw_usage);
    if let Some(state) = replay_state(
        previous.as_ref(),
        body.terminal_status.as_deref(),
        &usage,
        &artifacts,
        desired,
    )? {
        let row = previous.as_ref().ok_or_else(conflict)?;
        if !raw_changed && !promotes_target(previous.as_ref(), &context) {
            return Ok(metering_reply(
                &context,
                &state,
                &row.state,
                row.reason.as_deref(),
            ));
        }
        if matches!(row.state.as_str(), "settled" | "outbox_pending") {
            return Err(conflict());
        }
    }
    bind_artifacts(&mut *conn, state, dp, &body.target, &body.artifacts).await?;
    let reason = match body.reason {
        ReconciliationReason::MissingUsage => "missing_usage",
        ReconciliationReason::UnknownUsageShape => "unknown_usage_shape",
        ReconciliationReason::UnpricedMeter => "unpriced_meter",
        ReconciliationReason::IncompleteObservation => "incomplete_observation",
        ReconciliationReason::UncertainSubmission => "uncertain_submission",
    };
    persist(
        conn,
        state,
        dp,
        &context,
        previous.as_ref(),
        ReceiptDraft {
            desired_state: desired,
            terminal_status: body.terminal_status.as_deref(),
            reason: Some(reason),
            usage,
            artifacts,
            receipt: serde_json::to_value(&body).map_err(unavailable)?,
            raw_usage,
        },
    )
    .await?;
    Ok(metering_reply(
        &context,
        "reconciliation",
        desired,
        Some(reason),
    ))
}

#[derive(sqlx::FromRow)]
struct DeliveryRow {
    operation_id: Uuid,
    dp_id: Uuid,
    receipt_version: i64,
    delivery_unpriced: bool,
    reported_unpriced: bool,
    all_delivered: bool,
}

/// 只由既有 outbox 交付事务调用；缺价标记跨批次持久保留，不能由最后一批覆盖。
pub(super) async fn delivered(
    conn: &mut PgConnection,
    state: &AppState,
    operation_ids: &[Uuid],
    batch_unpriced: bool,
) -> Result<(), DbError> {
    let rows: Vec<DeliveryRow> = sqlx::query_as(
        "SELECT meter.operation_id,meter.dp_id,meter.receipt_version,meter.delivery_unpriced, \
         EXISTS(SELECT 1 FROM native_terminal_outbox outbox \
           JOIN native_objects object ON object.id=outbox.object_id \
           LEFT JOIN usage_events usage ON usage.event_id=outbox.event_id::text AND usage.dp_id=outbox.dp_id \
           LEFT JOIN pricing_charges_cny charge ON charge.event_id=usage.event_id \
           WHERE object.environment_id=meter.environment_id AND object.operation_id=meter.operation_id \
             AND outbox.delivered_at IS NOT NULL AND (charge.event_id IS NULL \
               OR charge.status NOT IN ('charged','exempt','nothing_to_bill') \
               OR charge.charge_micro_cny IS NULL \
               OR (charge.status='charged' AND charge.charge_micro_cny>0 \
                 AND NOT EXISTS(SELECT 1 FROM billing_ledger_cny ledger \
                   WHERE ledger.event_id=charge.event_id AND ledger.tenant_id=meter.tenant_id \
                     AND ledger.kind='usage' AND ledger.amount_micro_cny=-charge.charge_micro_cny)))) \
           AS reported_unpriced, \
         NOT EXISTS(SELECT 1 FROM native_terminal_outbox outbox \
           JOIN native_objects object ON object.id=outbox.object_id \
           WHERE object.environment_id=meter.environment_id AND object.operation_id=meter.operation_id \
             AND outbox.delivered_at IS NULL) AS all_delivered \
         FROM native_metering_records meter WHERE meter.environment_id=$1 \
         AND meter.operation_id=ANY($2) AND meter.state='outbox_pending' \
         AND EXISTS(SELECT 1 FROM native_terminal_outbox outbox \
           JOIN native_objects object ON object.id=outbox.object_id \
           WHERE object.environment_id=meter.environment_id AND object.operation_id=meter.operation_id) \
         ORDER BY meter.operation_id FOR UPDATE OF meter",
    ).bind(state.env.env_id).bind(operation_ids).fetch_all(&mut *conn).await?;
    for row in rows {
        let unpriced = row.delivery_unpriced || row.reported_unpriced;
        if !row.all_delivered && unpriced == row.delivery_unpriced {
            continue;
        }
        let next_state = if !row.all_delivered {
            "outbox_pending"
        } else if unpriced {
            "awaiting_reconciliation"
        } else {
            "settled"
        };
        let reason = (row.all_delivered && unpriced).then_some("unpriced_meter");
        let version = row.receipt_version.checked_add(1).ok_or_else(|| {
            DbError::Db(sqlx::Error::Protocol(
                "native metering receipt version is exhausted".into(),
            ))
        })?;
        sqlx::query("UPDATE native_metering_records SET state=$3,reason=$4,receipt_version=$5,delivery_unpriced=$6,updated_at=clock_timestamp() WHERE environment_id=$1 AND operation_id=$2")
            .bind(state.env.env_id).bind(row.operation_id).bind(next_state).bind(reason)
            .bind(version).bind(unpriced).execute(&mut *conn).await?;
        let receipt = json!({"type":"terminal_outbox_delivery", "batch_unpriced":batch_unpriced,
            "delivery_unpriced":unpriced,"all_terminal_events_delivered":row.all_delivered,
            "metering_state":next_state,"reconciliation_reason":reason});
        sqlx::query("INSERT INTO native_metering_receipts(environment_id,operation_id,version,observer_dp_id,state,receipt) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(state.env.env_id).bind(row.operation_id).bind(version).bind(row.dp_id)
            .bind(next_state).bind(receipt).execute(&mut *conn).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(state: &str, usage: Value) -> MeteringRow {
        MeteringRow {
            object_id: None,
            target_kind: "operation".into(),
            state: state.into(),
            terminal_status: Some("succeeded".into()),
            reason: Some("unpriced_meter".into()),
            observed_usage: usage,
            artifacts: json!([]),
            raw_usage: None,
            receipt_version: 1,
        }
    }

    fn training(tokens: u64) -> Value {
        json!([{"entry_id":"", "upstream_model":"training-model", "occurred_at":"2026-11-14T00:00:00Z",
            "meter":{"kind":"training_tokens","trained_tokens":tokens}}])
    }

    #[test]
    fn a_missing_observation_can_gain_real_usage_without_fabricating_zero() {
        let mut old = stored("unknown", json!([]));
        old.terminal_status = None;
        for tokens in [0, 3] {
            assert_eq!(
                replay_state(
                    Some(&old),
                    Some("succeeded"),
                    &training(tokens),
                    &json!([]),
                    "awaiting_reconciliation"
                )
                .unwrap(),
                None
            );
        }
        assert_eq!(
            old.observed_usage,
            json!([]),
            "the original missing receipt is immutable"
        );
    }

    #[test]
    fn reported_quantity_cannot_be_changed_or_downgraded_by_replay() {
        let old = stored("awaiting_reconciliation", training(3));
        assert_eq!(
            replay_state(
                Some(&old),
                Some("succeeded"),
                &training(3),
                &json!([]),
                "awaiting_reconciliation"
            )
            .unwrap(),
            Some("reconciliation".into())
        );
        assert!(replay_state(
            Some(&old),
            Some("succeeded"),
            &training(0),
            &json!([]),
            "awaiting_reconciliation"
        )
        .is_err());
        assert!(replay_state(
            Some(&old),
            Some("succeeded"),
            &json!([]),
            &json!([]),
            "unknown"
        )
        .is_err());
    }

    #[test]
    fn a_known_quantity_can_gain_its_terminal_status_without_changing_measurement() {
        let mut old = stored("awaiting_reconciliation", training(3));
        old.terminal_status = None;
        assert_eq!(
            replay_state(
                Some(&old),
                Some("succeeded"),
                &training(3),
                &json!([]),
                "awaiting_reconciliation"
            )
            .unwrap(),
            None
        );
        assert!(replay_state(
            Some(&old),
            Some("succeeded"),
            &training(4),
            &json!([]),
            "awaiting_reconciliation"
        )
        .is_err());
    }

    #[test]
    fn settled_replay_ignores_observer_time_but_rejects_a_new_quantity() {
        let old = stored("settled", training(3));
        let mut repeated = training(3);
        repeated[0]["occurred_at"] = json!("2026-11-15T00:00:00Z");
        assert_eq!(
            replay_state(
                Some(&old),
                Some("succeeded"),
                &repeated,
                &json!([]),
                "settled"
            )
            .unwrap(),
            Some("completed".into())
        );
        assert!(replay_state(
            Some(&old),
            Some("succeeded"),
            &training(4),
            &json!([]),
            "settled"
        )
        .is_err());
        assert!(replay_state(
            Some(&old),
            Some("failed"),
            &training(3),
            &json!([]),
            "settled"
        )
        .is_err());
    }

    #[test]
    fn pending_outbox_is_complete_but_cannot_claim_a_ledger_settlement() {
        let old = stored("outbox_pending", training(3));
        assert_eq!(
            replay_state(
                Some(&old),
                Some("succeeded"),
                &training(3),
                &json!([]),
                "awaiting_reconciliation"
            )
            .unwrap(),
            Some("completed".into())
        );
        assert_eq!(old.state, "outbox_pending");
        assert!(replay_state(
            Some(&old),
            Some("succeeded"),
            &training(4),
            &json!([]),
            "outbox_pending"
        )
        .is_err());
    }

    #[test]
    fn a_later_receipt_keeps_previously_bound_artifacts() {
        let artifact = json!({"entry_id":"item","root":{"kind":"file","raw_id":"file-original"}});
        let mut old = stored("unknown", json!([]));
        old.artifacts = json!([artifact.clone()]);
        assert_eq!(
            artifacts_union(Some(&old), json!([])).unwrap(),
            json!([artifact.clone()])
        );
        assert_eq!(
            artifacts_union(Some(&old), json!([artifact.clone(), artifact.clone()])).unwrap(),
            json!([artifact])
        );
    }
}
