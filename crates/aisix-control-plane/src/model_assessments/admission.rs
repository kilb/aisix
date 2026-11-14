//! 数据库固定两槽与账号唯一准入；锁顺序始终为槽、run、sample。

use crate::db::DbError;
use crate::error::ApiError;
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

pub(crate) async fn lock(conn: &mut PgConnection) -> Result<(), ApiError> {
    sqlx::query("SELECT slot FROM model_assessment_admission ORDER BY slot FOR UPDATE")
        .fetch_all(&mut *conn)
        .await
        .map_err(DbError::from)?;
    // 失联调用隔离到固定绝对截止加六十秒，心跳过期本身不释放槽。
    sqlx::query("UPDATE model_assessment_admission SET run_id=NULL,sample_id=NULL,account_id=NULL,owner_generation=NULL,dispatch_deadline_at=NULL,isolated_until=NULL WHERE isolated_until <= clock_timestamp()")
        .execute(conn).await.map_err(DbError::from)?;
    Ok(())
}

pub(crate) async fn reserve(
    conn: &mut PgConnection,
    run_id: Uuid,
    sample_id: Uuid,
    account_id: Uuid,
    generation: Uuid,
    deadline: DateTime<Utc>,
) -> Result<bool, ApiError> {
    let occupied: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM model_assessment_admission WHERE account_id=$1)",
    )
    .bind(account_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    if occupied {
        return Ok(false);
    }
    let row = sqlx::query(
        "SELECT slot FROM model_assessment_admission WHERE sample_id IS NULL ORDER BY slot LIMIT 1",
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let Some(row) = row else {
        return Ok(false);
    };
    sqlx::query("UPDATE model_assessment_admission SET run_id=$2,sample_id=$3,account_id=$4,owner_generation=$5,dispatch_deadline_at=$6,isolated_until=$6+interval '60 seconds' WHERE slot=$1 AND sample_id IS NULL")
        .bind(row.get::<i16,_>("slot")).bind(run_id).bind(sample_id).bind(account_id).bind(generation).bind(deadline)
        .execute(conn).await.map_err(DbError::from)?;
    Ok(true)
}

pub(crate) async fn release(
    conn: &mut PgConnection,
    sample_id: Uuid,
    generation: Uuid,
) -> Result<(), ApiError> {
    sqlx::query("UPDATE model_assessment_admission SET run_id=NULL,sample_id=NULL,account_id=NULL,owner_generation=NULL,dispatch_deadline_at=NULL,isolated_until=NULL WHERE sample_id=$1 AND owner_generation=$2")
        .bind(sample_id).bind(generation).execute(conn).await.map_err(DbError::from)?;
    Ok(())
}
