//! 集合关系以持久栅栏和父版本串行提交；发现不能复活已分离的旧关系。

use super::*;

#[derive(sqlx::FromRow)]
struct ReservationRow {
    operation_id: Uuid,
    dp_id: Uuid,
    api_key_id: Uuid,
    parent_id: Uuid,
    relation: String,
    action: String,
    parent_epoch: i64,
    parent_version: i64,
    child_ids: Vec<Uuid>,
    state: String,
    payload: Option<Value>,
}

#[derive(sqlx::FromRow)]
struct MembershipRow {
    id: Uuid,
    parent_id: Uuid,
    child_id: Uuid,
    relation: String,
    generation: i64,
    version: i64,
}

impl MembershipRow {
    fn view(self) -> Result<NativeMembership, ApiError> {
        Ok(NativeMembership {
            membership_id: self.id,
            parent_record_id: self.parent_id,
            child_record_id: self.child_id,
            relation: serde_json::from_value(json!(self.relation)).map_err(unavailable)?,
            generation: self.generation,
            version: self.version,
        })
    }
}

fn action_name(action: RelationAction) -> &'static str {
    match action {
        RelationAction::Attach => "attach",
        RelationAction::Detach => "detach",
        RelationAction::Discover => "discover",
    }
}

fn object_kind(row: &ObjectRow) -> Result<NativeObjectKind, ApiError> {
    serde_json::from_value(json!(row.kind)).map_err(unavailable)
}

fn object_protocol(row: &ObjectRow) -> Result<NativeProtocol, ApiError> {
    serde_json::from_value(json!(row.protocol)).map_err(unavailable)
}

fn parent_permitted(parent: &ObjectRow, relation: NativeRelation) -> Result<bool, ApiError> {
    let protocol = object_protocol(parent)?;
    let kind = object_kind(parent)?;
    use NativeObjectKind as K;
    Ok([
        K::ResponseItem,
        K::ResponseContainer,
        K::ContainerFile,
        K::File,
        K::VectorStoreFileBatch,
        K::EvalRun,
        K::EvalOutputItem,
        K::FineTuningCheckpoint,
        K::FineTunedModel,
        K::Video,
        K::VideoCharacter,
    ]
    .into_iter()
    .any(|child| relation.permits(protocol, kind, child)))
}

fn child_consumer(
    parent: &ObjectRow,
    child: &ObjectRow,
    requested: Option<EndpointFamily>,
) -> Result<Option<EndpointFamily>, ApiError> {
    let family = requested.unwrap_or(object_kind(parent)?.endpoint_family());
    let child_kind = object_kind(child)?;
    if child_kind.endpoint_family() == family {
        Ok(None)
    } else if native_object_consumer_allowed(object_protocol(child)?, child_kind, family) {
        Ok(Some(family))
    } else {
        Err(ApiError::not_found())
    }
}

fn same_account(parent: &ObjectRow, child: &ObjectRow) -> bool {
    parent.environment_id == child.environment_id
        && parent.tenant_id == child.tenant_id
        && parent.protocol == child.protocol
        && parent.provider_key_id == child.provider_key_id
        && parent.boundary == child.boundary
}

async fn child_visible(
    conn: &mut PgConnection,
    state: &AppState,
    caller: &Principal,
    parent: &ObjectRow,
    child: &ObjectRow,
    requested: Option<EndpointFamily>,
) -> Result<bool, ApiError> {
    let consumer = match child_consumer(parent, child, requested) {
        Ok(consumer) => consumer,
        Err(error) if error.status == axum::http::StatusCode::NOT_FOUND => return Ok(false),
        Err(error) => return Err(error),
    };
    match authorized_for_consumer(conn, state, caller, child, consumer).await {
        Ok(()) => Ok(true),
        Err(error) if error.status == axum::http::StatusCode::NOT_FOUND => Ok(false),
        Err(error) => Err(error),
    }
}

async fn lock_operation(
    conn: &mut PgConnection,
    state: &AppState,
    operation_id: Uuid,
) -> Result<(), ApiError> {
    let identity = format!("native-relation:{}:{operation_id}", state.env.env_id);
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
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
    lock: bool,
) -> Result<Option<ReservationRow>, ApiError> {
    sqlx::query_as(&format!(
        "SELECT operation_id,dp_id,api_key_id,parent_id,relation,action,parent_epoch,\
         parent_version,child_ids,state,payload FROM native_relation_reservations \
         WHERE environment_id=$1 AND operation_id=$2{}",
        if lock { " FOR UPDATE" } else { "" },
    ))
    .bind(state.env.env_id)
    .bind(operation_id)
    .fetch_optional(conn)
    .await
    .map_err(unavailable)
}

async fn lock_objects(
    conn: &mut PgConnection,
    state: &AppState,
    parent_id: Uuid,
    child_ids: &[Uuid],
) -> Result<(ObjectRow, Vec<ObjectRow>), ApiError> {
    let mut ids = BTreeSet::from([parent_id]);
    ids.extend(child_ids.iter().copied());
    let ids: Vec<Uuid> = ids.into_iter().collect();
    let mut rows: Vec<ObjectRow> = sqlx::query_as(
        "SELECT * FROM native_objects WHERE environment_id=$1 AND id=ANY($2) \
         ORDER BY id FOR UPDATE",
    )
    .bind(state.env.env_id)
    .bind(&ids)
    .fetch_all(conn)
    .await
    .map_err(unavailable)?;
    if rows.len() != ids.len() {
        return Err(ApiError::not_found());
    }
    let index = rows
        .iter()
        .position(|row| row.id == parent_id)
        .ok_or_else(ApiError::not_found)?;
    let parent = rows.remove(index);
    Ok((parent, rows))
}

async fn record_view(
    conn: &mut PgConnection,
    state: &AppState,
    row: &ObjectRow,
) -> Result<NativeRecord, ApiError> {
    let submitted = submission(&operation(conn, state, row.operation_id, false).await?)?;
    row.view(&submitted)
}

pub(super) async fn reserve(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: ReserveRelationsRequest,
) -> Result<NativeReply, ApiError> {
    if body.expected_parent_version < 1
        || body
            .child_record_ids
            .len()
            .saturating_add(body.references.len())
            > MAX_ENTRIES
        || body
            .references
            .iter()
            .map(|reference| reference.record_id)
            .collect::<BTreeSet<_>>()
            .len()
            != body.references.len()
        || body.child_record_ids.len() > MAX_ENTRIES
        || body.child_record_ids.contains(&body.parent_record_id)
        || body.child_record_ids.iter().collect::<BTreeSet<_>>().len()
            != body.child_record_ids.len()
        || (body.action == RelationAction::Detach && body.child_record_ids.is_empty())
        || (body.action == RelationAction::Discover && !body.child_record_ids.is_empty())
    {
        return Err(invalid("/relations"));
    }
    // 租户锁先于提交与对象锁；同一操作的重放使用自己的事务锁。
    let caller = principal(&mut *conn, body.api_key_id).await?;
    lock_operation(&mut *conn, state, body.operation_id).await?;
    let mut admitted_ids = body.child_record_ids.clone();
    admitted_ids.extend(body.references.iter().map(|reference| reference.record_id));
    let (parent, admitted) =
        lock_objects(&mut *conn, state, body.parent_record_id, &admitted_ids).await?;
    let children: Vec<_> = admitted
        .iter()
        .filter(|row| body.child_record_ids.contains(&row.id))
        .collect();
    let mut reference_identity = Vec::new();
    for reference in &body.references {
        let family = serde_json::to_value(reference.consumer_family).map_err(unavailable)?;
        reference_identity.push((
            reference.record_id,
            family
                .as_str()
                .ok_or_else(|| invalid("/references/consumer_family"))?
                .to_owned(),
        ));
    }
    reference_identity.sort_by_key(|reference| reference.0);
    authorized(&mut *conn, state, &caller, &parent).await?;
    if !parent_permitted(&parent, body.relation)? {
        return Err(invalid("/relation"));
    }
    if let Some(previous) = reservation(&mut *conn, state, body.operation_id, true).await? {
        if previous.dp_id != dp.dp_id
            || previous.api_key_id != body.api_key_id
            || previous.parent_id != body.parent_record_id
            || previous.parent_version != body.expected_parent_version
            || previous.relation != body.relation.as_str()
            || previous.action != action_name(body.action)
            || previous.child_ids != body.child_record_ids
        {
            return Err(conflict());
        }
        let stored_references: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT record_id,consumer_family FROM native_operation_references \
             WHERE environment_id=$1 AND operation_id=$2 ORDER BY record_id",
        )
        .bind(state.env.env_id)
        .bind(body.operation_id)
        .fetch_all(&mut *conn)
        .await
        .map_err(unavailable)?;
        if stored_references != reference_identity {
            return Err(conflict());
        }
        let mut reply = NativeReply::new(&previous.state);
        reply.operation_id = Some(previous.operation_id);
        return Ok(reply);
    }
    if parent.version != body.expected_parent_version {
        return Err(conflict());
    }
    let family = object_kind(&parent)?.endpoint_family();
    for reference in &body.references {
        let referenced = if reference.record_id == parent.id {
            &parent
        } else {
            admitted
                .iter()
                .find(|row| row.id == reference.record_id)
                .ok_or_else(ApiError::not_found)?
        };
        let kind = object_kind(referenced)?;
        if reference.consumer_family != family
            || (kind.endpoint_family() != family
                && !native_object_consumer_allowed(object_protocol(referenced)?, kind, family))
        {
            return Err(invalid("/references/consumer_family"));
        }
        if !same_account(&parent, referenced) {
            return Err(ApiError::not_found());
        }
        authorized_for_consumer(&mut *conn, state, &caller, referenced, Some(family)).await?;
    }
    for child in &children {
        if !same_account(&parent, child)
            || !body.relation.permits(
                object_protocol(&parent)?,
                object_kind(&parent)?,
                object_kind(child)?,
            )
        {
            return Err(ApiError::not_found());
        }
        let consumer = child_consumer(&parent, child, None)?;
        authorized_for_consumer(&mut *conn, state, &caller, child, consumer).await?;
        if body.action == RelationAction::Detach {
            let active: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM native_object_memberships \
                 WHERE environment_id=$1 AND parent_id=$2 AND child_id=$3 \
                 AND relation=$4 AND deleted_at IS NULL)",
            )
            .bind(state.env.env_id)
            .bind(parent.id)
            .bind(child.id)
            .bind(body.relation.as_str())
            .fetch_one(&mut *conn)
            .await
            .map_err(unavailable)?;
            if !active {
                return Err(ApiError::not_found());
            }
        }
    }
    if body.action != RelationAction::Discover {
        let fenced: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM native_parent_fences WHERE environment_id=$1 \
             AND parent_id=$2 AND state IN ('held','uncertain') AND operation_id<>$3)",
        )
        .bind(state.env.env_id)
        .bind(parent.id)
        .bind(body.operation_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(unavailable)?;
        if fenced {
            return Err(conflict());
        }
        sqlx::query(
            "INSERT INTO native_parent_fences(environment_id,parent_id,operation_id,state) \
             VALUES($1,$2,$3,'held') ON CONFLICT DO NOTHING",
        )
        .bind(state.env.env_id)
        .bind(parent.id)
        .bind(body.operation_id)
        .execute(&mut *conn)
        .await
        .map_err(unavailable)?;
    }
    super::foundation::create_management_operation(
        &mut *conn,
        state,
        dp,
        &caller,
        &parent,
        body.operation_id,
        "relation_mutation",
    )
    .await?;
    for (record_id, family) in reference_identity {
        let referenced = if record_id == parent.id {
            &parent
        } else {
            admitted
                .iter()
                .find(|row| row.id == record_id)
                .ok_or_else(ApiError::not_found)?
        };
        sqlx::query(
            "INSERT INTO native_operation_references(environment_id,operation_id,record_id,\
             record_version,parent_epoch,consumer_family) VALUES($1,$2,$3,$4,$5,$6)",
        )
        .bind(state.env.env_id)
        .bind(body.operation_id)
        .bind(record_id)
        .bind(referenced.version)
        .bind(referenced.membership_epoch)
        .bind(family)
        .execute(&mut *conn)
        .await
        .map_err(unavailable)?;
    }
    sqlx::query(
        "INSERT INTO native_relation_reservations(environment_id,operation_id,dp_id,api_key_id,\
         parent_id,relation,action,parent_epoch,parent_version,child_ids,state) \
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'reserved')",
    )
    .bind(state.env.env_id)
    .bind(body.operation_id)
    .bind(dp.dp_id)
    .bind(body.api_key_id)
    .bind(parent.id)
    .bind(body.relation.as_str())
    .bind(action_name(body.action))
    .bind(parent.membership_epoch)
    .bind(parent.version)
    .bind(&body.child_record_ids)
    .execute(&mut *conn)
    .await
    .map_err(unavailable)?;
    let mut reply = NativeReply::new("reserved");
    reply.operation_id = Some(body.operation_id);
    reply.record = Some(record_view(conn, state, &parent).await?);
    Ok(reply)
}

pub(super) async fn commit(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: CommitRelationsRequest,
) -> Result<NativeReply, ApiError> {
    if body.objects.len() > MAX_ENTRIES
        || body
            .objects
            .iter()
            .any(|object| !text_ok(&object.raw_id, 512))
        || body
            .objects
            .iter()
            .map(|object| (object.kind.as_str(), object.raw_id.as_str()))
            .collect::<BTreeSet<_>>()
            .len()
            != body.objects.len()
    {
        return Err(invalid("/objects"));
    }
    // 先无锁识别读取型发现，再按租户、操作、对象顺序稳定当前权限。
    let previous = reservation(&mut *conn, state, body.operation_id, false)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if previous.dp_id != dp.dp_id || previous.api_key_id != body.api_key_id {
        return Err(ApiError::not_found());
    }
    let discoverer = if previous.action == "discover" {
        Some(principal(&mut *conn, body.api_key_id).await?)
    } else {
        None
    };
    lock_operation(&mut *conn, state, body.operation_id).await?;
    let reservation = reservation(&mut *conn, state, body.operation_id, true)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if reservation.dp_id != dp.dp_id || reservation.api_key_id != body.api_key_id {
        return Err(ApiError::not_found());
    }
    if reservation.action != previous.action {
        return Err(conflict());
    }
    let request = serde_json::to_value(&body).map_err(unavailable)?;
    if reservation.state == "committed" {
        let payload = reservation.payload.ok_or_else(conflict)?;
        if payload.get("request") != Some(&request) {
            return Err(conflict());
        }
        let mut reply: NativeReply =
            serde_json::from_value(payload.get("reply").cloned().ok_or_else(conflict)?)
                .map_err(unavailable)?;
        if let Some(caller) = &discoverer {
            // 幂等回包不重新写关系，但每次读取都重新裁决父子对象的当前权限。
            let child_ids: Vec<_> = reply
                .records
                .iter()
                .map(|record| record.record_id)
                .collect();
            let parent = object(&mut *conn, state, reservation.parent_id).await?;
            authorized(&mut *conn, state, caller, &parent).await?;
            // 归属不变的子对象沿用关系读取的无锁查询，不从子对象倒序锁父对象。
            let children: Vec<ObjectRow> = sqlx::query_as(
                "SELECT * FROM native_objects WHERE environment_id=$1 AND id=ANY($2) ORDER BY id",
            )
            .bind(state.env.env_id)
            .bind(&child_ids)
            .fetch_all(&mut *conn)
            .await
            .map_err(unavailable)?;
            let mut visible = BTreeSet::new();
            for child in &children {
                if child_visible(&mut *conn, state, caller, &parent, child, None).await? {
                    visible.insert(child.id);
                }
            }
            reply
                .records
                .retain(|record| visible.contains(&record.record_id));
            reply
                .memberships
                .retain(|membership| visible.contains(&membership.child_record_id));
        }
        return Ok(reply);
    }
    if !matches!(reservation.state.as_str(), "reserved" | "uncertain") {
        return Err(conflict());
    }
    if reservation
        .child_ids
        .len()
        .saturating_add(body.objects.len())
        > MAX_ENTRIES
    {
        return Err(invalid("/objects"));
    }
    let relation: NativeRelation =
        serde_json::from_value(json!(reservation.relation)).map_err(unavailable)?;
    let proofs: Vec<(Uuid, i64, i64)> = sqlx::query_as(
        "SELECT record_id,record_version,parent_epoch FROM native_operation_references \
         WHERE environment_id=$1 AND operation_id=$2 ORDER BY record_id",
    )
    .bind(state.env.env_id)
    .bind(reservation.operation_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(unavailable)?;
    let mut admitted_ids = reservation.child_ids.clone();
    admitted_ids.extend(proofs.iter().map(|proof| proof.0));
    let (parent, admitted) =
        lock_objects(&mut *conn, state, reservation.parent_id, &admitted_ids).await?;
    if let Some(caller) = &discoverer {
        authorized(&mut *conn, state, caller, &parent).await?;
    }
    for (id, version, epoch) in proofs {
        let referenced = if id == parent.id {
            &parent
        } else {
            admitted
                .iter()
                .find(|row| row.id == id)
                .ok_or_else(conflict)?
        };
        if referenced.deleted_at.is_some()
            || referenced.version != version
            || referenced.membership_epoch != epoch
        {
            return Err(conflict());
        }
    }
    let mut children: Vec<_> = admitted
        .into_iter()
        .filter(|row| reservation.child_ids.contains(&row.id))
        .collect();
    if parent.deleted_at.is_some()
        || parent.version != reservation.parent_version
        || parent.membership_epoch != reservation.parent_epoch
        || !parent_permitted(&parent, relation)?
        || (reservation.action == "detach" && !body.objects.is_empty())
    {
        return Err(conflict());
    }
    let other_fence: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM native_parent_fences WHERE environment_id=$1 \
         AND parent_id=$2 AND operation_id<>$3 AND state IN ('held','uncertain'))",
    )
    .bind(state.env.env_id)
    .bind(parent.id)
    .bind(reservation.operation_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(unavailable)?;
    if other_fence {
        return Err(conflict());
    }
    if reservation.action != "discover" {
        let own_fence: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM native_parent_fences WHERE environment_id=$1 \
             AND parent_id=$2 AND operation_id=$3 AND state IN ('held','uncertain'))",
        )
        .bind(state.env.env_id)
        .bind(parent.id)
        .bind(reservation.operation_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(unavailable)?;
        if !own_fence {
            return Err(conflict());
        }
    }
    let submitted = submission(&operation(&mut *conn, state, parent.operation_id, false).await?)?;
    let caller_scope = operation(&mut *conn, state, reservation.operation_id, false).await?;
    let mut observed_objects: Vec<_> = body.objects.iter().collect();
    observed_objects.sort_by(|left, right| {
        (left.kind.as_str(), &left.raw_id).cmp(&(right.kind.as_str(), &right.raw_id))
    });
    for observed in observed_objects {
        if !relation.permits(
            object_protocol(&parent)?,
            object_kind(&parent)?,
            observed.kind,
        ) {
            return Err(invalid("/objects/kind"));
        }
        if children
            .iter()
            .any(|child| child.kind == observed.kind.as_str() && child.raw_id == observed.raw_id)
        {
            continue;
        }
        // 已准入的活关系可复用原子对象，不能因 Full 曾附加另一成员的 File 而改写其 owner。
        let related:Option<ObjectRow>=sqlx::query_as("SELECT child.* FROM native_object_memberships membership JOIN native_objects child ON child.id=membership.child_id WHERE membership.environment_id=$1 AND membership.parent_id=$2 AND membership.relation=$3 AND membership.deleted_at IS NULL AND child.protocol=$4 AND child.kind=$5 AND child.boundary=$6 AND child.raw_id=$7 AND child.deleted_at IS NULL FOR UPDATE OF child")
            .bind(state.env.env_id).bind(parent.id).bind(relation.as_str()).bind(&parent.protocol).bind(observed.kind.as_str()).bind(&parent.boundary).bind(&observed.raw_id).fetch_optional(&mut *conn).await.map_err(unavailable)?;
        if let Some(related) = related {
            let visible = discoverer.as_ref().map_or_else(
                || {
                    caller_scope.visibility_full
                        || related.api_key_id == caller_scope.api_key_id
                        || (caller_scope.member_id.is_some()
                            && related.member_id == caller_scope.member_id)
                },
                |caller| owns(caller, &related),
            );
            if !visible {
                if reservation.action == "discover" {
                    continue;
                }
                return Err(ApiError::not_found());
            }
            if !children.iter().any(|child| child.id == related.id) {
                children.push(related);
            }
            continue;
        }
        // owner 来自已准入父对象，Full 调用人的第一次 list 不能抢走 Self 产物。
        let child = match derive_object(
            &mut *conn,
            state,
            &parent,
            &submitted,
            observed,
            None,
            discoverer.as_ref(),
        )
        .await
        {
            Ok(child) => child,
            Err(error)
                if discoverer.is_some() && error.status == axum::http::StatusCode::NOT_FOUND =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        if !children.iter().any(|previous| previous.id == child.id) {
            children.push(child);
        }
    }
    children.sort_by_key(|child| child.id);
    let mut reply = NativeReply::new("committed");
    reply.operation_id = Some(reservation.operation_id);
    for child in &children {
        if child.deleted_at.is_some()
            || !same_account(&parent, child)
            || !relation.permits(
                object_protocol(&parent)?,
                object_kind(&parent)?,
                object_kind(child)?,
            )
        {
            return Err(conflict());
        }
        if let Some(caller) = &discoverer {
            if !child_visible(&mut *conn, state, caller, &parent, child, None).await? {
                continue;
            }
        }
        let membership = if reservation.action == "detach" {
            let removed: Option<MembershipRow> = sqlx::query_as(
                "UPDATE native_object_memberships SET deleted_at=clock_timestamp(),version=version+1 \
                 WHERE environment_id=$1 AND parent_id=$2 AND child_id=$3 AND relation=$4 \
                 AND deleted_at IS NULL RETURNING id,parent_id,child_id,relation,generation,version",
            )
            .bind(state.env.env_id)
            .bind(parent.id)
            .bind(child.id)
            .bind(relation.as_str())
            .fetch_optional(&mut *conn)
            .await
            .map_err(unavailable)?;
            removed.ok_or_else(conflict)?.view()?
        } else {
            if reservation.action == "discover" {
                // 删除后的公共/延迟列表不是重新 attach 的凭据，必须有新准入写操作。
                let old_detach: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM native_object_memberships \
                     WHERE environment_id=$1 AND parent_id=$2 AND child_id=$3 AND relation=$4 \
                     AND deleted_at IS NOT NULL) AND NOT EXISTS(SELECT 1 FROM native_object_memberships \
                     WHERE environment_id=$1 AND parent_id=$2 AND child_id=$3 AND relation=$4 \
                     AND deleted_at IS NULL)",
                )
                .bind(state.env.env_id)
                .bind(parent.id)
                .bind(child.id)
                .bind(relation.as_str())
                .fetch_one(&mut *conn)
                .await
                .map_err(unavailable)?;
                if old_detach {
                    return Err(conflict());
                }
            }
            insert_membership(
                &mut *conn,
                state,
                &parent,
                child,
                relation,
                reservation.operation_id,
            )
            .await?
        };
        reply.memberships.push(membership);
        reply
            .records
            .push(record_view(&mut *conn, state, child).await?);
    }
    let parent: ObjectRow = sqlx::query_as(
        "UPDATE native_objects SET membership_epoch=membership_epoch+1,version=version+1 \
         WHERE environment_id=$1 AND id=$2 AND version=$3 AND membership_epoch=$4 RETURNING *",
    )
    .bind(state.env.env_id)
    .bind(parent.id)
    .bind(reservation.parent_version)
    .bind(reservation.parent_epoch)
    .fetch_optional(&mut *conn)
    .await
    .map_err(unavailable)?
    .ok_or_else(conflict)?;
    reply.record = Some(record_view(&mut *conn, state, &parent).await?);
    sqlx::query(
        "UPDATE native_relation_reservations SET state='committed',payload=$3 \
         WHERE environment_id=$1 AND operation_id=$2",
    )
    .bind(state.env.env_id)
    .bind(reservation.operation_id)
    .bind(json!({"request": request, "reply": serde_json::to_value(&reply).map_err(unavailable)?}))
    .execute(&mut *conn)
    .await
    .map_err(unavailable)?;
    sqlx::query(
        "UPDATE native_operations SET state='bound' WHERE environment_id=$1 AND operation_id=$2",
    )
    .bind(state.env.env_id)
    .bind(reservation.operation_id)
    .execute(&mut *conn)
    .await
    .map_err(unavailable)?;
    sqlx::query(
        "UPDATE native_parent_fences SET state='released' WHERE environment_id=$1 \
         AND parent_id=$2 AND operation_id=$3 AND state IN ('held','uncertain')",
    )
    .bind(state.env.env_id)
    .bind(parent.id)
    .bind(reservation.operation_id)
    .execute(conn)
    .await
    .map_err(unavailable)?;
    Ok(reply)
}

pub(super) async fn lookup(
    conn: &mut PgConnection,
    state: &AppState,
    _dp: DpIdentity,
    body: LookupRelationsRequest,
) -> Result<NativeReply, ApiError> {
    if body.raw_ids.len() > MAX_PAGE || body.raw_ids.iter().any(|id| !text_ok(id, 512)) {
        return Err(invalid("/raw_ids"));
    }
    let caller = principal(&mut *conn, body.api_key_id).await?;
    let parent = object(&mut *conn, state, body.parent_record_id).await?;
    let parent_kind = object_kind(&parent)?;
    let parent_protocol = object_protocol(&parent)?;
    if body.consumer_family.is_some_and(|family| {
        family != parent_kind.endpoint_family()
            && !native_object_consumer_allowed(parent_protocol, parent_kind, family)
    }) {
        return Err(invalid("/consumer_family"));
    }
    authorized_for_consumer(&mut *conn, state, &caller, &parent, body.consumer_family).await?;
    if !parent_permitted(&parent, body.relation)? {
        return Err(invalid("/relation"));
    }
    let rows: Vec<MembershipRow> = sqlx::query_as(
        "SELECT m.id,m.parent_id,m.child_id,m.relation,m.generation,m.version \
         FROM native_object_memberships m JOIN native_objects child ON child.id=m.child_id \
         AND child.environment_id=m.environment_id WHERE m.environment_id=$1 \
         AND m.parent_id=$2 AND m.relation=$3 AND m.deleted_at IS NULL \
         AND child.raw_id=ANY($4) ORDER BY child.id,m.id",
    )
    .bind(state.env.env_id)
    .bind(parent.id)
    .bind(body.relation.as_str())
    .bind(&body.raw_ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(unavailable)?;
    let mut reply = NativeReply::new("related");
    for membership in rows {
        let child: ObjectRow =
            sqlx::query_as("SELECT * FROM native_objects WHERE environment_id=$1 AND id=$2")
                .bind(state.env.env_id)
                .bind(membership.child_id)
                .fetch_one(&mut *conn)
                .await
                .map_err(unavailable)?;
        if !same_account(&parent, &child)
            || !body
                .relation
                .permits(object_protocol(&parent)?, parent_kind, object_kind(&child)?)
        {
            return Err(ApiError::unavailable());
        }
        if !child_visible(
            &mut *conn,
            state,
            &caller,
            &parent,
            &child,
            body.consumer_family,
        )
        .await?
        {
            continue;
        }
        reply
            .records
            .push(record_view(&mut *conn, state, &child).await?);
        reply.memberships.push(membership.view()?);
    }
    Ok(reply)
}

pub(super) async fn resolve(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: ResolveReservationRequest,
) -> Result<NativeReply, ApiError> {
    lock_operation(&mut *conn, state, body.operation_id).await?;
    let previous = reservation(&mut *conn, state, body.operation_id, true)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if previous.dp_id != dp.dp_id || previous.api_key_id != body.api_key_id {
        return Err(ApiError::not_found());
    }
    if previous.state == "committed" {
        return Err(conflict());
    }
    if previous.state == "rejected" {
        return if body.upstream_rejected {
            Ok(NativeReply::new("rejected"))
        } else {
            Err(conflict())
        };
    }
    let (state_name, fence_state) = if body.upstream_rejected {
        ("rejected", "released")
    } else {
        ("uncertain", "uncertain")
    };
    sqlx::query(
        "UPDATE native_relation_reservations SET state=$3 WHERE environment_id=$1 AND operation_id=$2",
    )
    .bind(state.env.env_id)
    .bind(body.operation_id)
    .bind(state_name)
    .execute(&mut *conn)
    .await
    .map_err(unavailable)?;
    if !body.upstream_rejected {
        sqlx::query(
            "UPDATE native_operations SET state='uncertain' WHERE environment_id=$1 AND operation_id=$2",
        )
        .bind(state.env.env_id)
        .bind(body.operation_id)
        .execute(&mut *conn)
        .await
        .map_err(unavailable)?;
    }
    sqlx::query(
        "UPDATE native_parent_fences SET state=$4 WHERE environment_id=$1 AND parent_id=$2 \
         AND operation_id=$3 AND state IN ('held','uncertain')",
    )
    .bind(state.env.env_id)
    .bind(previous.parent_id)
    .bind(body.operation_id)
    .bind(fence_state)
    .execute(conn)
    .await
    .map_err(unavailable)?;
    Ok(NativeReply::new(state_name))
}
