//! 删除先持久保留对象栅栏；终态账务和独立子对象不随根对象删除。

use super::*;

#[derive(sqlx::FromRow)]
struct DeleteReservation {
    dp_id: Uuid,
    api_key_id: Uuid,
    record_id: Uuid,
    object_version: i64,
    state: String,
}

async fn lock_operation(
    conn: &mut PgConnection,
    state: &AppState,
    operation_id: Uuid,
) -> Result<(), ApiError> {
    let identity = format!("native-delete:{}:{operation_id}", state.env.env_id);
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(identity)
        .execute(conn)
        .await
        .map_err(unavailable)?;
    Ok(())
}

async fn reservation(
    conn: &mut PgConnection,
    state: &AppState,
    operation_id: Uuid,
) -> Result<Option<DeleteReservation>, ApiError> {
    sqlx::query_as(
        "SELECT dp_id,api_key_id,record_id,object_version,state FROM native_delete_reservations \
         WHERE environment_id=$1 AND operation_id=$2 FOR UPDATE",
    )
    .bind(state.env.env_id)
    .bind(operation_id)
    .fetch_optional(conn)
    .await
    .map_err(unavailable)
}

fn same_request(
    reservation: &DeleteReservation,
    dp: DpIdentity,
    body: &ObjectDeletionRequest,
) -> bool {
    reservation.dp_id == dp.dp_id
        && reservation.api_key_id == body.api_key_id
        && reservation.record_id == body.record_id
        && reservation.object_version == body.version
}

async fn protect_ledger(
    conn: &mut PgConnection,
    state: &AppState,
    row: &ObjectRow,
) -> Result<(), ApiError> {
    let pending: bool =
        sqlx::query_scalar("SELECT pending FROM native_objects WHERE environment_id=$1 AND id=$2")
            .bind(state.env.env_id)
            .bind(row.id)
            .fetch_one(&mut *conn)
            .await
            .map_err(unavailable)?;
    if row.completed_at.is_none()
        && (matches!(
            row.kind.as_str(),
            "message_batch" | "fine_tuning_job" | "video" | "eval_run"
        ) || (row.kind == "response" && pending))
    {
        // 缺失末次用量和待对账都不等同于已完成，不能先删掉唯一上游观察源。
        return Err(conflict());
    }
    if row.kind == "eval" {
        let unfinished: bool = sqlx::query_scalar(
            "WITH RECURSIVE edges(parent_id,child_id) AS ( \
             SELECT parent_id,id FROM native_objects WHERE environment_id=$1 AND parent_id IS NOT NULL \
             UNION SELECT parent_id,child_id FROM native_object_memberships \
             WHERE environment_id=$1 AND deleted_at IS NULL), \
             descendants(id) AS (SELECT $2::uuid UNION \
             SELECT edges.child_id FROM edges JOIN descendants ON edges.parent_id=descendants.id) \
             SELECT EXISTS(SELECT 1 FROM native_objects child JOIN descendants ON child.id=descendants.id \
             WHERE child.environment_id=$1 AND child.kind='eval_run' AND child.pending \
             AND child.completed_at IS NULL)",
        )
        .bind(state.env.env_id)
        .bind(row.id)
        .fetch_one(conn)
        .await
        .map_err(unavailable)?;
        if unfinished {
            return Err(conflict());
        }
    }
    Ok(())
}

pub(super) async fn reserve(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: ObjectDeletionRequest,
) -> Result<NativeReply, ApiError> {
    if body.version < 1 {
        return Err(invalid("/version"));
    }
    let caller = principal(&mut *conn, body.api_key_id).await?;
    lock_operation(&mut *conn, state, body.operation_id).await?;
    if let Some(previous) = reservation(&mut *conn, state, body.operation_id).await? {
        if !same_request(&previous, dp, &body) {
            return Err(conflict());
        }
        let mut reply = NativeReply::new(if previous.state == "committed" {
            "deleted"
        } else {
            &previous.state
        });
        reply.operation_id = Some(body.operation_id);
        return Ok(reply);
    }
    let row = object(&mut *conn, state, body.record_id).await?;
    authorized(&mut *conn, state, &caller, &row).await?;
    if row.version != body.version {
        return Err(conflict());
    }
    super::foundation::assert_unfenced(&mut *conn, state, row.id, Some(body.operation_id)).await?;
    protect_ledger(&mut *conn, state, &row).await?;
    super::foundation::create_management_operation(
        &mut *conn,
        state,
        dp,
        &caller,
        &row,
        body.operation_id,
        "deletion",
    )
    .await?;
    sqlx::query(
        "INSERT INTO native_parent_fences(environment_id,parent_id,operation_id,state) \
         VALUES($1,$2,$3,'held') ON CONFLICT DO NOTHING",
    )
    .bind(state.env.env_id)
    .bind(row.id)
    .bind(body.operation_id)
    .execute(&mut *conn)
    .await
    .map_err(unavailable)?;
    sqlx::query(
        "INSERT INTO native_delete_reservations(environment_id,operation_id,dp_id,api_key_id,\
         record_id,object_version,state) VALUES($1,$2,$3,$4,$5,$6,'reserved')",
    )
    .bind(state.env.env_id)
    .bind(body.operation_id)
    .bind(dp.dp_id)
    .bind(body.api_key_id)
    .bind(row.id)
    .bind(body.version)
    .execute(&mut *conn)
    .await
    .map_err(unavailable)?;
    let accepted = submission(&operation(&mut *conn, state, row.operation_id, false).await?)?;
    let mut reply = NativeReply::new("reserved");
    reply.operation_id = Some(body.operation_id);
    reply.record = Some(row.view(&accepted)?);
    Ok(reply)
}

pub(super) async fn commit(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: ObjectDeletionRequest,
) -> Result<NativeReply, ApiError> {
    lock_operation(&mut *conn, state, body.operation_id).await?;
    let previous = reservation(&mut *conn, state, body.operation_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if previous.dp_id != dp.dp_id || previous.api_key_id != body.api_key_id {
        return Err(ApiError::not_found());
    }
    if !same_request(&previous, dp, &body) {
        return Err(conflict());
    }
    if previous.state == "committed" {
        return Ok(NativeReply::new("deleted"));
    }
    if !matches!(previous.state.as_str(), "reserved" | "uncertain") {
        return Err(conflict());
    }
    // 受理后的成功删除必须落盘；这里不借用 worker 身份重验已撤销的原密钥。
    let row = object(&mut *conn, state, previous.record_id).await?;
    if row.deleted_at.is_some() || row.version != previous.object_version {
        return Err(conflict());
    }
    super::foundation::assert_unfenced(&mut *conn, state, row.id, Some(body.operation_id)).await?;
    let fenced: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM native_parent_fences WHERE environment_id=$1 \
         AND parent_id=$2 AND operation_id=$3 AND state IN ('held','uncertain'))",
    )
    .bind(state.env.env_id)
    .bind(row.id)
    .bind(body.operation_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(unavailable)?;
    if !fenced {
        return Err(conflict());
    }
    protect_ledger(&mut *conn, state, &row).await?;
    sqlx::query(
        "UPDATE native_object_memberships SET deleted_at=clock_timestamp(),version=version+1 \
         WHERE environment_id=$1 AND parent_id=$2 AND deleted_at IS NULL",
    )
    .bind(state.env.env_id)
    .bind(row.id)
    .execute(&mut *conn)
    .await
    .map_err(unavailable)?;
    let changed = sqlx::query(
        "UPDATE native_objects SET deleted_at=clock_timestamp(),membership_epoch=membership_epoch+1,\
         version=version+1 WHERE environment_id=$1 AND id=$2 AND version=$3 AND deleted_at IS NULL",
    )
    .bind(state.env.env_id)
    .bind(row.id)
    .bind(previous.object_version)
    .execute(&mut *conn)
    .await
    .map_err(unavailable)?;
    if changed.rows_affected() != 1 {
        return Err(conflict());
    }
    sqlx::query(
        "UPDATE native_delete_reservations SET state='committed' \
         WHERE environment_id=$1 AND operation_id=$2",
    )
    .bind(state.env.env_id)
    .bind(body.operation_id)
    .execute(&mut *conn)
    .await
    .map_err(unavailable)?;
    sqlx::query(
        "UPDATE native_operations SET state='bound' WHERE environment_id=$1 AND operation_id=$2",
    )
    .bind(state.env.env_id)
    .bind(body.operation_id)
    .execute(&mut *conn)
    .await
    .map_err(unavailable)?;
    sqlx::query(
        "UPDATE native_parent_fences SET state='released' WHERE environment_id=$1 \
         AND parent_id=$2 AND operation_id=$3 AND state IN ('held','uncertain')",
    )
    .bind(state.env.env_id)
    .bind(row.id)
    .bind(body.operation_id)
    .execute(conn)
    .await
    .map_err(unavailable)?;
    Ok(NativeReply::new("deleted"))
}
