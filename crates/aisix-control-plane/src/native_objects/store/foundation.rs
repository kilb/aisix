//! 已接受身份与对象关系一起持久化；产物的所有者不由发现它的调用人决定。
use super::*;

pub(super) async fn assert_unfenced(
    conn: &mut PgConnection,
    state: &AppState,
    parent: Uuid,
    own: Option<Uuid>,
) -> Result<(), ApiError> {
    let busy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM native_parent_fences WHERE environment_id=$1 AND parent_id=$2 AND state IN ('held','uncertain') AND ($3::uuid IS NULL OR operation_id <> $3))")
        .bind(state.env.env_id).bind(parent).bind(own).fetch_one(conn).await.map_err(unavailable)?;
    if busy {
        return Err(conflict());
    }
    Ok(())
}

pub(super) async fn create_management_operation(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    caller: &Principal,
    parent: &ObjectRow,
    operation_id: Uuid,
    operation_name: &str,
) -> Result<(), ApiError> {
    let original = operation(&mut *conn, state, parent.operation_id, false).await?;
    let mut accepted = submission(&original)?;
    accepted.operation_id = operation_id;
    accepted.api_key_id = caller.key.id;
    accepted.parent_id = Some(parent.id);
    accepted.asynchronous = false;
    accepted.entries.clear();
    accepted.operation = operation_name.to_owned();
    let wire = serde_json::to_value(accepted).map_err(unavailable)?;
    sqlx::query("INSERT INTO native_operations(environment_id,operation_id,dp_id,tenant_id,api_key_id,member_id,visibility_full,submission) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING")
        .bind(state.env.env_id).bind(operation_id).bind(dp.dp_id).bind(caller.tenant.id).bind(caller.key.id).bind(caller.key.member_id).bind(caller.full).bind(&wire).execute(&mut *conn).await.map_err(unavailable)?;
    let stored = operation(conn, state, operation_id, true).await?;
    if stored.dp_id != dp.dp_id || stored.api_key_id != caller.key.id || stored.submission != wire {
        return Err(conflict());
    }
    Ok(())
}

pub(super) async fn derive_object(
    conn: &mut PgConnection,
    state: &AppState,
    owner: &ObjectRow,
    accepted: &BeginRequest,
    child: &NativeChildObject,
    parent_id: Option<Uuid>,
    discoverer: Option<&Principal>,
) -> Result<ObjectRow, ApiError> {
    let kind = child.kind;
    let raw_id = child.raw_id.as_str();
    if !text_ok(raw_id, 512) {
        return Err(invalid("/objects/raw_id"));
    }
    let identity = serde_json::to_string(&(
        state.env.env_id,
        accepted.protocol,
        kind,
        &accepted.origin.boundary,
        raw_id,
    ))
    .map_err(unavailable)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(&identity)
        .execute(&mut *conn)
        .await
        .map_err(unavailable)?;
    let owner_op = operation(&mut *conn, state, owner.operation_id, false).await?;
    let existing: Option<ObjectRow> = sqlx::query_as("SELECT * FROM native_objects WHERE environment_id=$1 AND protocol=$2 AND kind=$3 AND boundary=$4 AND raw_id=$5 FOR UPDATE")
        .bind(state.env.env_id).bind(accepted.protocol.as_str()).bind(kind.as_str()).bind(accepted.origin.boundary.as_str()).bind(raw_id).fetch_optional(&mut *conn).await.map_err(unavailable)?;
    if let Some(row) = existing {
        // 发现复用当前读取者的可见性；已接受产物仍按原 owner 的历史快照落库。
        let visible = discoverer.map_or_else(
            || {
                owner_op.visibility_full
                    || row.api_key_id == owner.api_key_id
                    || (owner.member_id.is_some() && row.member_id == owner.member_id)
            },
            |caller| owns(caller, &row),
        );
        if row.tenant_id != owner.tenant_id
            || !visible
            || row.provider_key_id != accepted.origin.provider_key_id
            || row.deleted_at.is_some()
        {
            return Err(if discoverer.is_some() {
                ApiError::not_found()
            } else {
                conflict()
            });
        }
        return Ok(row);
    }
    let operation_id = Uuid::new_v5(&accepted.operation_id, identity.as_bytes());
    let mut derived = accepted.clone();
    derived.operation_id = operation_id;
    derived.api_key_id = owner.api_key_id;
    derived.origin.surface = Some(
        accepted
            .origin
            .surface
            .unwrap_or(native_origin_surface(accepted.protocol, accepted.kind)),
    );
    derived.kind = kind;
    derived.parent_id = parent_id;
    derived.input_file_id = None;
    derived.asynchronous = false;
    derived.entries.clear();
    derived.operation = kind.as_str().to_owned();
    sqlx::query("INSERT INTO native_operations(environment_id,operation_id,dp_id,tenant_id,api_key_id,member_id,visibility_full,submission,state) VALUES($1,$2,$3,$4,$5,$6,$7,$8,'bound')")
        .bind(state.env.env_id).bind(operation_id).bind(owner_op.dp_id).bind(owner.tenant_id).bind(owner.api_key_id).bind(owner.member_id).bind(owner_op.visibility_full).bind(serde_json::to_value(&derived).map_err(unavailable)?).execute(&mut *conn).await.map_err(unavailable)?;
    sqlx::query("INSERT INTO native_operation_origin_sources(environment_id,operation_id,source_record_id) VALUES($1,$2,$3)")
        .bind(state.env.env_id).bind(operation_id).bind(owner.id).execute(&mut *conn).await.map_err(unavailable)?;
    sqlx::query_as("INSERT INTO native_objects(id,environment_id,operation_id,tenant_id,api_key_id,member_id,protocol,kind,model_id,provider_key_id,boundary,raw_id,parent_id,pending) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,false) RETURNING *")
        .bind(Uuid::new_v4()).bind(state.env.env_id).bind(operation_id).bind(owner.tenant_id).bind(owner.api_key_id).bind(owner.member_id).bind(accepted.protocol.as_str()).bind(kind.as_str()).bind(accepted.origin.model_id).bind(accepted.origin.provider_key_id).bind(accepted.origin.boundary.as_str()).bind(raw_id).bind(parent_id).fetch_one(conn).await.map_err(unavailable)
}

pub(super) async fn insert_membership(
    conn: &mut PgConnection,
    state: &AppState,
    parent: &ObjectRow,
    child: &ObjectRow,
    relation: NativeRelation,
    operation_id: Uuid,
) -> Result<NativeMembership, ApiError> {
    let protocol: NativeProtocol =
        serde_json::from_value(json!(parent.protocol)).map_err(unavailable)?;
    let parent_kind: NativeObjectKind =
        serde_json::from_value(json!(parent.kind)).map_err(unavailable)?;
    let child_kind: NativeObjectKind =
        serde_json::from_value(json!(child.kind)).map_err(unavailable)?;
    if !relation.permits(protocol, parent_kind, child_kind)
        || parent.tenant_id != child.tenant_id
        || parent.provider_key_id != child.provider_key_id
        || parent.boundary != child.boundary
        || parent.deleted_at.is_some()
        || child.deleted_at.is_some()
    {
        return Err(conflict());
    }
    let current: Option<(Uuid,i64,i64)> = sqlx::query_as("SELECT id,generation,version FROM native_object_memberships WHERE environment_id=$1 AND parent_id=$2 AND child_id=$3 AND relation=$4 AND deleted_at IS NULL FOR UPDATE")
        .bind(state.env.env_id).bind(parent.id).bind(child.id).bind(relation.as_str()).fetch_optional(&mut *conn).await.map_err(unavailable)?;
    let (id, generation, version) = match current {
        Some(row) => row,
        None => {
            let generation: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(generation),0)+1 FROM native_object_memberships WHERE environment_id=$1 AND parent_id=$2 AND child_id=$3 AND relation=$4")
                .bind(state.env.env_id).bind(parent.id).bind(child.id).bind(relation.as_str()).fetch_one(&mut *conn).await.map_err(unavailable)?;
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO native_object_memberships(id,environment_id,parent_id,child_id,relation,created_operation_id,generation) VALUES($1,$2,$3,$4,$5,$6,$7)")
                .bind(id).bind(state.env.env_id).bind(parent.id).bind(child.id).bind(relation.as_str()).bind(operation_id).bind(generation).execute(&mut *conn).await.map_err(unavailable)?;
            (id, generation, 1)
        }
    };
    Ok(NativeMembership {
        membership_id: id,
        parent_record_id: parent.id,
        child_record_id: child.id,
        relation,
        generation,
        version,
    })
}

pub(super) async fn begin_related(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    mut body: BeginRelatedRequest,
) -> Result<NativeReply, ApiError> {
    if body
        .lifecycle_context
        .as_ref()
        .is_some_and(|context| context.validate().is_err())
    {
        return Err(invalid("/lifecycle_context"));
    }
    if body.references.len() > MAX_ENTRIES
        || body.owner_parent_record_id.is_some_and(|id| {
            Some(id) != body.submission.parent_id
                && body.submission.kind != NativeObjectKind::MessageContainer
        })
    {
        return Err(invalid("/references"));
    }
    body.references.sort_by_key(|r| r.record_id);
    if body
        .references
        .windows(2)
        .any(|r| r[0].record_id == r[1].record_id)
    {
        return Err(invalid("/references"));
    }
    let principal = principal(&mut *conn, body.submission.api_key_id).await?;
    let mut rows = Vec::new();
    for reference in &body.references {
        let row = object(&mut *conn, state, reference.record_id).await?;
        let protocol: NativeProtocol =
            serde_json::from_value(json!(row.protocol)).map_err(unavailable)?;
        let kind: NativeObjectKind =
            serde_json::from_value(json!(row.kind)).map_err(unavailable)?;
        if !native_object_consumer_allowed(protocol, kind, reference.consumer_family)
            && reference.consumer_family != kind.endpoint_family()
        {
            return Err(invalid("/references/consumer_family"));
        }
        authorized_for_consumer(
            &mut *conn,
            state,
            &principal,
            &row,
            Some(reference.consumer_family),
        )
        .await?;
        if row.provider_key_id != body.submission.origin.provider_key_id
            || row.boundary != body.submission.origin.boundary.as_str()
        {
            return Err(conflict());
        }
        assert_unfenced(
            &mut *conn,
            state,
            row.id,
            Some(body.submission.operation_id),
        )
        .await?;
        rows.push(row);
    }
    if body
        .owner_parent_record_id
        .is_some_and(|id| !rows.iter().any(|r| r.id == id))
    {
        return Err(invalid("/owner_parent_record_id"));
    }
    if body.submission.kind == NativeObjectKind::MessageContainer
        && body.owner_parent_record_id.is_some_and(|id| {
            rows.iter()
                .all(|row| row.id != id || row.kind != "message_container")
        })
    {
        return Err(invalid("/owner_parent_record_id"));
    }
    let reply = begin(&mut *conn, state, dp, body.submission.clone()).await?;
    let wire = serde_json::to_value(&body).map_err(unavailable)?;
    sqlx::query("INSERT INTO native_related_operations(environment_id,operation_id,manifest,owner_record_id) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING")
        .bind(state.env.env_id).bind(body.submission.operation_id).bind(&wire).bind(body.owner_parent_record_id).execute(&mut *conn).await.map_err(unavailable)?;
    let stored: Value=sqlx::query_scalar("SELECT manifest FROM native_related_operations WHERE environment_id=$1 AND operation_id=$2 FOR UPDATE").bind(state.env.env_id).bind(body.submission.operation_id).fetch_one(&mut *conn).await.map_err(unavailable)?;
    let stored_body: BeginRelatedRequest = serde_json::from_value(stored).map_err(unavailable)?;
    let mut canonical_stored = serde_json::to_value(stored_body).map_err(unavailable)?;
    let mut canonical_wire = wire;
    for value in [&mut canonical_stored, &mut canonical_wire] {
        if let Some(origin) = value
            .pointer_mut("/submission/origin")
            .and_then(Value::as_object_mut)
        {
            origin.remove("surface");
        }
    }
    if canonical_stored != canonical_wire {
        return Err(conflict());
    }
    for (reference, row) in body.references.iter().zip(rows) {
        sqlx::query("INSERT INTO native_operation_references(environment_id,operation_id,record_id,record_version,parent_epoch,consumer_family) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING")
            .bind(state.env.env_id).bind(body.submission.operation_id).bind(row.id).bind(row.version).bind(row.membership_epoch).bind(serde_json::to_value(reference.consumer_family).map_err(unavailable)?.as_str().ok_or_else(||invalid("/consumer_family"))?).execute(&mut *conn).await.map_err(unavailable)?;
        sqlx::query("INSERT INTO native_parent_fences(environment_id,parent_id,operation_id,state) VALUES($1,$2,$3,'held') ON CONFLICT DO NOTHING")
            .bind(state.env.env_id).bind(row.id).bind(body.submission.operation_id).execute(&mut *conn).await.map_err(unavailable)?;
    }
    Ok(reply)
}

async fn root_object(
    conn: &mut PgConnection,
    state: &AppState,
    op: &OperationRow,
    submitted: &BeginRequest,
    raw_id: &str,
    owner: Option<&ObjectRow>,
    references: &BTreeSet<Uuid>,
) -> Result<ObjectRow, ApiError> {
    if !text_ok(raw_id, 512) {
        return Err(invalid("/raw_id"));
    }
    let (tenant, key, member) = owner.map_or((op.tenant_id, op.api_key_id, op.member_id), |r| {
        (r.tenant_id, r.api_key_id, r.member_id)
    });
    let existing: Option<ObjectRow> = sqlx::query_as(
        "SELECT * FROM native_objects WHERE environment_id=$1 AND operation_id=$2 FOR UPDATE",
    )
    .bind(state.env.env_id)
    .bind(submitted.operation_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(unavailable)?;
    if let Some(row) = existing {
        if row.raw_id != raw_id
            || row.tenant_id != tenant
            || row.api_key_id != key
            || row.deleted_at.is_some()
        {
            return Err(conflict());
        }
        return Ok(row);
    }
    if submitted.kind == NativeObjectKind::MessageContainer {
        let existing:Option<ObjectRow>=sqlx::query_as("SELECT * FROM native_objects WHERE environment_id=$1 AND protocol=$2 AND kind='message_container' AND boundary=$3 AND raw_id=$4 FOR UPDATE")
            .bind(state.env.env_id).bind(submitted.protocol.as_str()).bind(submitted.origin.boundary.as_str()).bind(raw_id).fetch_optional(&mut *conn).await.map_err(unavailable)?;
        if let Some(existing) = existing {
            if references.contains(&existing.id)
                && existing.tenant_id == op.tenant_id
                && existing.provider_key_id == submitted.origin.provider_key_id
                && existing.deleted_at.is_none()
            {
                return Ok(existing);
            }
            return Err(conflict());
        }
    }
    sqlx::query_as("INSERT INTO native_objects(id,environment_id,operation_id,tenant_id,api_key_id,member_id,protocol,kind,model_id,provider_key_id,boundary,raw_id,parent_id,pending) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) RETURNING *")
        .bind(Uuid::new_v4()).bind(state.env.env_id).bind(submitted.operation_id).bind(tenant).bind(key).bind(member).bind(submitted.protocol.as_str()).bind(submitted.kind.as_str()).bind(submitted.origin.model_id).bind(submitted.origin.provider_key_id).bind(submitted.origin.boundary.as_str()).bind(raw_id).bind(submitted.parent_id).bind(submitted.asynchronous).fetch_one(conn).await.map_err(|error|if error.as_database_error().is_some_and(|db|db.is_unique_violation()){conflict()}else{unavailable(error)})
}

async fn resolve_selector(
    conn: &mut PgConnection,
    state: &AppState,
    selector: &ArtifactSelector,
    root: &ObjectRow,
    children: &[ObjectRow],
    references: &BTreeSet<Uuid>,
) -> Result<ObjectRow, ApiError> {
    let id = match selector {
        ArtifactSelector::Root => root.id,
        ArtifactSelector::Record { record_id } => {
            if !references.contains(record_id) {
                return Err(invalid("/relations/record_id"));
            }
            *record_id
        }
        ArtifactSelector::Observed { kind, raw_id } => children
            .iter()
            .find(|r| r.kind == kind.as_str() && r.raw_id == *raw_id)
            .map(|r| r.id)
            .ok_or_else(|| invalid("/relations/observed"))?,
    };
    object(conn, state, id).await
}

async fn bind_graph(
    conn: &mut PgConnection,
    state: &AppState,
    root: &ObjectRow,
    submitted: &BeginRequest,
    objects: Vec<NativeChildObject>,
    relations: Vec<BoundRelation>,
    references: &BTreeSet<Uuid>,
    operation_id: Uuid,
) -> Result<NativeReply, ApiError> {
    if objects.len() > MAX_ENTRIES || relations.len() > MAX_ENTRIES {
        return Err(invalid("/objects"));
    }
    let mut children = Vec::new();
    let mut objects = objects;
    objects.sort_by(|a, b| (a.kind.as_str(), &a.raw_id).cmp(&(b.kind.as_str(), &b.raw_id)));
    for child in objects {
        if !relations.iter().any(|r|matches!(&r.child,ArtifactSelector::Observed{kind,raw_id} if *kind==child.kind && *raw_id==child.raw_id)) {return Err(invalid("/objects"));}
        let provenance_parent = (root.kind == "response"
            && matches!(
                child.kind,
                NativeObjectKind::ResponseItem | NativeObjectKind::ResponseContainer
            ))
        .then_some(root.id);
        children.push(
            derive_object(
                &mut *conn,
                state,
                root,
                submitted,
                &child,
                provenance_parent,
                None,
            )
            .await?,
        );
    }
    let mut reply = NativeReply::new("bound");
    for relation in relations {
        let parent = resolve_selector(
            &mut *conn,
            state,
            &relation.parent,
            root,
            &children,
            references,
        )
        .await?;
        let child = resolve_selector(
            &mut *conn,
            state,
            &relation.child,
            root,
            &children,
            references,
        )
        .await?;
        assert_unfenced(&mut *conn, state, parent.id, Some(operation_id)).await?;
        reply.memberships.push(
            insert_membership(
                &mut *conn,
                state,
                &parent,
                &child,
                relation.relation,
                operation_id,
            )
            .await?,
        );
        sqlx::query("UPDATE native_objects SET membership_epoch=membership_epoch+1,version=version+1 WHERE id=$1").bind(parent.id).execute(&mut *conn).await.map_err(unavailable)?;
    }
    let current_root = object(&mut *conn, state, root.id).await?;
    let mut root_view = current_root.view(submitted)?;
    enrich_record(&mut *conn, state, &mut root_view).await?;
    reply.record = Some(root_view);
    for child in children {
        let child_op = operation(&mut *conn, state, child.operation_id, false).await?;
        reply.records.push(child.view(&submission(&child_op)?)?);
    }
    Ok(reply)
}

pub(super) async fn bind_related(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: BindRelatedRequest,
) -> Result<NativeReply, ApiError> {
    let op = operation(&mut *conn, state, body.binding.operation_id, true).await?;
    if op.dp_id != dp.dp_id
        || op.api_key_id != body.binding.api_key_id
        || body.binding.existing_record_id.is_some()
    {
        return Err(ApiError::not_found());
    }
    let accepted = submission(&op)?;
    let row:Option<(Value,Option<Uuid>,Option<Value>)>=sqlx::query_as("SELECT manifest,owner_record_id,bound_reply FROM native_related_operations WHERE environment_id=$1 AND operation_id=$2 FOR UPDATE").bind(state.env.env_id).bind(body.binding.operation_id).fetch_optional(&mut *conn).await.map_err(unavailable)?;
    let (_, owner_id, receipt) = row.ok_or_else(ApiError::not_found)?;
    let request = serde_json::to_value(&body).map_err(unavailable)?;
    if let Some(receipt) = receipt {
        if receipt.get("request") != Some(&request) {
            return Err(conflict());
        }
        return serde_json::from_value(receipt["reply"].clone()).map_err(unavailable);
    }
    let references:Vec<(Uuid,i64,i64)>=sqlx::query_as("SELECT record_id,record_version,parent_epoch FROM native_operation_references WHERE environment_id=$1 AND operation_id=$2 ORDER BY record_id").bind(state.env.env_id).bind(body.binding.operation_id).fetch_all(&mut *conn).await.map_err(unavailable)?;
    let mut reference_ids = BTreeSet::new();
    for (id, version, epoch) in references {
        let reference = object(&mut *conn, state, id).await?;
        if reference.deleted_at.is_some()
            || reference.version != version
            || reference.membership_epoch != epoch
        {
            return Err(conflict());
        }
        reference_ids.insert(id);
    }
    let owner = match owner_id {
        Some(id) => Some(object(&mut *conn, state, id).await?),
        None => None,
    };
    let root = root_object(
        &mut *conn,
        state,
        &op,
        &accepted,
        &body.binding.raw_id,
        owner.as_ref(),
        &reference_ids,
    )
    .await?;
    let root_accepted = if root.operation_id == accepted.operation_id {
        accepted.clone()
    } else {
        submission(&operation(&mut *conn, state, root.operation_id, false).await?)?
    };
    let reply = bind_graph(
        &mut *conn,
        state,
        &root,
        &root_accepted,
        body.binding.objects,
        body.relations,
        &reference_ids,
        body.binding.operation_id,
    )
    .await?;
    sqlx::query(
        "UPDATE native_operations SET state='bound' WHERE environment_id=$1 AND operation_id=$2",
    )
    .bind(state.env.env_id)
    .bind(body.binding.operation_id)
    .execute(&mut *conn)
    .await
    .map_err(unavailable)?;
    sqlx::query("UPDATE native_related_operations SET bound_reply=$3 WHERE environment_id=$1 AND operation_id=$2").bind(state.env.env_id).bind(body.binding.operation_id).bind(json!({"request":request,"reply":reply})).execute(&mut *conn).await.map_err(unavailable)?;
    sqlx::query("UPDATE native_parent_fences SET state='released' WHERE environment_id=$1 AND operation_id=$2").bind(state.env.env_id).bind(body.binding.operation_id).execute(&mut *conn).await.map_err(unavailable)?;
    Ok(reply)
}

pub(super) async fn resolve(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: ResolveReservationRequest,
) -> Result<NativeReply, ApiError> {
    let relation_exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM native_relation_reservations WHERE environment_id=$1 AND operation_id=$2)").bind(state.env.env_id).bind(body.operation_id).fetch_one(&mut *conn).await.map_err(unavailable)?;
    if relation_exists {
        return super::relations::resolve(conn, state, dp, body).await;
    }
    let op = operation(&mut *conn, state, body.operation_id, true).await?;
    if op.dp_id != dp.dp_id || op.api_key_id != body.api_key_id {
        return Err(ApiError::not_found());
    }
    let state_name = if body.upstream_rejected {
        "released"
    } else {
        "uncertain"
    };
    sqlx::query("UPDATE native_delete_reservations SET state=$3 WHERE environment_id=$1 AND operation_id=$2 AND state<>'committed'")
        .bind(state.env.env_id).bind(body.operation_id).bind(if body.upstream_rejected {"rejected"} else {"uncertain"}).execute(&mut *conn).await.map_err(unavailable)?;
    sqlx::query("UPDATE native_parent_fences SET state=$3 WHERE environment_id=$1 AND operation_id=$2 AND state<>'released'").bind(state.env.env_id).bind(body.operation_id).bind(state_name).execute(&mut *conn).await.map_err(unavailable)?;
    if !body.upstream_rejected {
        sqlx::query("UPDATE native_operations SET state='uncertain' WHERE environment_id=$1 AND operation_id=$2 AND state<>'bound'").bind(state.env.env_id).bind(body.operation_id).execute(&mut *conn).await.map_err(unavailable)?;
    }
    Ok(NativeReply::new(if body.upstream_rejected {
        "rejected"
    } else {
        "uncertain"
    }))
}

pub(super) async fn observed(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: BindObservedArtifactsRequest,
) -> Result<NativeReply, ApiError> {
    let owner = lease_live(&mut *conn, state, dp, &body.lease).await?;
    let op = operation(&mut *conn, state, owner.operation_id, false).await?;
    let accepted = submission(&op)?;
    if body.entries.len() > MAX_ENTRIES
        || body.complete.as_ref().is_some_and(|c| {
            c.lease.record_id != body.lease.record_id || c.lease.generation != body.lease.generation
        })
    {
        return Err(invalid("/entries"));
    }
    let mut reply = NativeReply::new("observed");
    let mut seen = BTreeSet::new();
    for entry in body.entries {
        if !seen.insert(entry.entry_id.clone()) {
            return Err(invalid("/entries"));
        }
        let entry_payload = serde_json::to_value(&entry).map_err(unavailable)?;
        let prior:Option<(Value,Value)>=sqlx::query_as("SELECT payload,reply FROM native_observed_entries WHERE environment_id=$1 AND owner_record_id=$2 AND entry_id=$3")
            .bind(state.env.env_id).bind(owner.id).bind(&entry.entry_id).fetch_optional(&mut *conn).await.map_err(unavailable)?;
        if let Some((payload, saved)) = prior {
            if payload != entry_payload {
                return Err(conflict());
            }
            let saved: NativeReply = serde_json::from_value(saved).map_err(unavailable)?;
            if let Some(root) = saved.record {
                reply.records.push(root);
            }
            reply.records.extend(saved.records);
            reply.memberships.extend(saved.memberships);
            continue;
        }
        let entry_id = entry.entry_id.clone();
        let mut per_entry = accepted.clone();
        if accepted.kind == NativeObjectKind::MessageBatch {
            let manifest = accepted
                .entries
                .iter()
                .find(|e| e.entry_id == entry.entry_id)
                .ok_or_else(|| invalid("/entries/entry_id"))?;
            if accepted.protocol != NativeProtocol::Openai
                || manifest.endpoint != "/v1/responses"
                || entry.root.kind != NativeObjectKind::Response
            {
                return Err(invalid("/entries/root"));
            }
            per_entry.operation_id = Uuid::new_v5(
                &owner.operation_id,
                format!("accepted-entry:{}", entry.entry_id).as_bytes(),
            );
            per_entry.origin.surface = Some(
                accepted
                    .origin
                    .surface
                    .unwrap_or(native_origin_surface(accepted.protocol, accepted.kind)),
            );
            per_entry.origin.model_id = manifest.model_id;
            per_entry.requested_model_id = manifest.requested_model_id;
            per_entry.requested_model = manifest.requested_model.clone();
            per_entry.upstream_model = manifest.upstream_model.clone();
            per_entry.upstream_provider = manifest.upstream_provider.clone();
            per_entry.entries.clear();
        } else if !entry.entry_id.is_empty()
            || entry.root.kind != accepted.kind
            || entry.root.raw_id != owner.raw_id
        {
            return Err(invalid("/entries/root"));
        }
        let root = if accepted.kind == NativeObjectKind::MessageBatch {
            derive_object(
                &mut *conn,
                state,
                &owner,
                &per_entry,
                &entry.root,
                None,
                None,
            )
            .await?
        } else {
            object(&mut *conn, state, owner.id).await?
        };
        if accepted.kind == NativeObjectKind::MessageBatch {
            let identity = serde_json::to_string(&(
                state.env.env_id,
                per_entry.protocol,
                entry.root.kind,
                &per_entry.origin.boundary,
                &entry.root.raw_id,
            ))
            .map_err(unavailable)?;
            let expected_operation = Uuid::new_v5(&per_entry.operation_id, identity.as_bytes());
            if root.operation_id != expected_operation {
                return Err(conflict());
            }
        }
        let root_op = operation(&mut *conn, state, root.operation_id, false).await?;
        let root_submission = submission(&root_op)?;
        let mut relations = entry.relations;
        // 旧 Responses 执行器的 typed children 自动归入同一受控产物关系。
        if root.kind == "response" && relations.is_empty() {
            for child in &entry.objects {
                relations.push(BoundRelation {
                    parent: ArtifactSelector::Root,
                    child: ArtifactSelector::Observed {
                        kind: child.kind,
                        raw_id: child.raw_id.clone(),
                    },
                    relation: NativeRelation::ResponseOutput,
                });
            }
        }
        let bound = bind_graph(
            &mut *conn,
            state,
            &root,
            &root_submission,
            entry.objects,
            relations,
            &BTreeSet::new(),
            root.operation_id,
        )
        .await?;
        sqlx::query("INSERT INTO native_observed_entries(environment_id,owner_record_id,entry_id,payload,reply) VALUES($1,$2,$3,$4,$5)")
            .bind(state.env.env_id).bind(owner.id).bind(entry_id).bind(entry_payload).bind(serde_json::to_value(&bound).map_err(unavailable)?).execute(&mut *conn).await.map_err(unavailable)?;
        if let Some(root_view) = bound.record {
            reply.records.push(root_view);
        }
        reply.records.extend(bound.records);
        reply.memberships.extend(bound.memberships);
    }
    if let Some(completion) = body.complete {
        complete(&mut *conn, state, dp, completion).await?;
    }
    let current = object(&mut *conn, state, owner.id).await?;
    reply.record = Some(current.view(&accepted)?);
    Ok(reply)
}

pub(super) async fn enrich_record(
    conn: &mut PgConnection,
    state: &AppState,
    record: &mut NativeRecord,
) -> Result<(), ApiError> {
    if let Some(operation_id) = record.operation_id {
        let context:Option<Value>=sqlx::query_scalar("SELECT manifest->'lifecycle_context' FROM native_related_operations WHERE environment_id=$1 AND operation_id=$2")
            .bind(state.env.env_id).bind(operation_id).fetch_optional(&mut *conn).await.map_err(unavailable)?;
        if let Some(context) = context.filter(|value| !value.is_null()) {
            record.lifecycle_context = Some(serde_json::from_value(context).map_err(unavailable)?);
        }
        let metering:Option<(String,Option<String>)>=sqlx::query_as("SELECT state,reason FROM native_metering_records WHERE environment_id=$1 AND operation_id=$2")
            .bind(state.env.env_id).bind(operation_id).fetch_optional(&mut *conn).await.map_err(unavailable)?;
        if let Some((metering_state, reason)) = metering {
            record.metering_state = Some(metering_state);
            record.reconciliation_reason = reason;
        }
    }
    Ok(())
}
