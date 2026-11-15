//! 整批输出的必要父范围是一跳，不是任意祖先授权闭包。
use super::*;

struct BatchFiles {
    batch: NativeRecord,
    result: NativeRecord,
    contextual: Option<NativeRecord>,
    ordinary: NativeRecord,
}

async fn completed_batch_files(f: &Fixture, with_context: bool) -> BatchFiles {
    let mut source = f.begin("key-a", NativeObjectKind::MessageBatch, true);
    source.entries = [("first", "model-a"), ("second", "model-b")]
        .into_iter()
        .map(|(entry, model)| NativeEntry {
            usage_context: None,
            entry_id: entry.into(),
            endpoint: "/v1/chat/completions".into(),
            requested_model_id: stable_resource_id("models", model),
            requested_model: model.into(),
            requested_service_tier: None,
            model_id: stable_resource_id("models", model),
            upstream_model: "test-model".into(),
            upstream_provider: "openai".into(),
            upstream_input_text: None,
        })
        .collect();
    f.store.begin(source.clone()).await.unwrap();
    let root = f
        .store
        .bind(BindRequest {
            operation_id: source.operation_id,
            api_key_id: source.api_key_id,
            raw_id: "batch-basic-file-context".into(),
            existing_record_id: None,
            objects: vec![],
            provider_created_at: None,
            provider_expires_at: None,
        })
        .await
        .unwrap()
        .record
        .unwrap();
    let leased = f
        .store
        .claim(ClaimRequest {
            limit: 1,
            record_id: Some(root.record_id),
            protocol: None,
            kind: None,
        })
        .await
        .unwrap()
        .records
        .remove(0);
    assert_eq!(
        f.store
            .complete(CompleteRequest {
                lease: LeaseToken {
                    record_id: root.record_id,
                    generation: leased.lease_generation.unwrap(),
                },
                terminal_status: "completed".into(),
                usage: vec![terminal_usage("first"), terminal_usage("second")],
                output_file_ids: vec!["file-basic-batch-result".into()],
            })
            .await
            .unwrap()
            .state,
        "completed"
    );
    let batch = f
        .store
        .lookup(lookup(
            "key-a",
            NativeObjectKind::MessageBatch,
            &root.raw_id,
        ))
        .await
        .unwrap()
        .record
        .unwrap();
    assert!(batch.completed);
    assert_eq!(batch.entries.len(), 2);
    let result = f
        .store
        .lookup(lookup(
            "key-a",
            NativeObjectKind::File,
            "file-basic-batch-result",
        ))
        .await
        .unwrap()
        .record
        .unwrap();
    assert_eq!(result.parent_id, Some(batch.record_id));
    assert_eq!(result.api_key_id, batch.api_key_id);
    assert_eq!(result.entries.len(), 2);

    let contextual = if with_context {
        // 已发布的 File 消费上下文可只带父 Batch，不在 File 上重复整批 entries。
        let mut child = f.begin("key-a", NativeObjectKind::File, false);
        child.parent_id = Some(batch.record_id);
        assert!(child.entries.is_empty());
        f.store.begin(child.clone()).await.unwrap();
        let contextual = f
            .store
            .bind(BindRequest {
                operation_id: child.operation_id,
                api_key_id: child.api_key_id,
                raw_id: "file-basic-contextual-result".into(),
                existing_record_id: None,
                objects: vec![],
                provider_created_at: None,
                provider_expires_at: None,
            })
            .await
            .unwrap()
            .record
            .unwrap();
        assert_eq!(contextual.parent_id, Some(batch.record_id));
        assert!(contextual.entries.is_empty());
        Some(contextual)
    } else {
        None
    };
    let ordinary = f
        .create(
            "key-a",
            NativeObjectKind::File,
            "file-basic-independent",
            false,
        )
        .await;
    BatchFiles {
        batch,
        result,
        contextual,
        ordinary,
    }
}

fn contextual_lookup(file: &NativeRecord) -> LookupRequest {
    let mut request = lookup("key-a", NativeObjectKind::File, &file.raw_id);
    request.consumer_family = Some(EndpointFamily::Batches);
    request
}

async fn healthy_and_foreign_controls(f: &Fixture, files: &BatchFiles) {
    for file in std::iter::once(&files.result).chain(files.contextual.iter()) {
        let request = if files
            .contextual
            .as_ref()
            .is_some_and(|contextual| contextual.record_id == file.record_id)
        {
            contextual_lookup(file)
        } else {
            lookup("key-a", NativeObjectKind::File, &file.raw_id)
        };
        assert_eq!(
            f.store
                .lookup(request)
                .await
                .unwrap()
                .record
                .unwrap()
                .record_id,
            file.record_id
        );
        assert_eq!(
            f.store
                .lookup(lookup("key-b", NativeObjectKind::File, &file.raw_id))
                .await
                .unwrap_err(),
            Error::NotFound
        );
    }
    let original = (*f.snapshot.load()).clone();
    let changed = original.clone();
    let mut provider = (*changed.provider_keys.get_by_id("provider-a").unwrap().value).clone();
    provider.api_key = "test-only-other-upstream-account".into();
    changed
        .provider_keys
        .insert(ResourceEntry::new("provider-a", provider, 2));
    f.snapshot.store(changed);
    for file in std::iter::once(&files.result).chain(files.contextual.iter()) {
        assert_eq!(
            f.store
                .lookup(lookup("key-a", NativeObjectKind::File, &file.raw_id))
                .await
                .unwrap_err(),
            Error::NotFound
        );
    }
    f.snapshot.store(original);
    for file in std::iter::once(&files.result).chain(files.contextual.iter()) {
        assert_eq!(
            f.store
                .lookup(lookup("key-a", NativeObjectKind::File, &file.raw_id))
                .await
                .unwrap()
                .record
                .unwrap()
                .record_id,
            file.record_id
        );
    }
}

async fn durable_facts(f: &Fixture) -> (Vec<(String, String, String)>, Vec<(String, String)>) {
    let objects =
        sqlx::query_as("SELECT record_id,owner,record FROM native_objects ORDER BY record_id")
            .fetch_all(&f.store.pool)
            .await
            .unwrap();
    let terminals =
        sqlx::query_as("SELECT event_id,event FROM native_terminal_outbox ORDER BY event_id")
            .fetch_all(&f.store.pool)
            .await
            .unwrap();
    (objects, terminals)
}

#[tokio::test]
async fn batch_contextual_file_keeps_the_secondary_model_grant_without_an_ancestor_closure() {
    let f = Fixture::new().await;
    let files = completed_batch_files(&f, true).await;
    healthy_and_foreign_controls(&f, &files).await;
    let before = durable_facts(&f).await;
    assert_eq!(before.1.len(), 2);
    let narrowed = (*f.snapshot.load()).clone();
    let mut key = (*narrowed.apikeys.get_by_id("key-a").unwrap().value).clone();
    key.allowed_models = vec!["model-a".into()];
    narrowed.apikeys.insert(ResourceEntry::new("key-a", key, 2));
    f.snapshot.store(narrowed);
    assert_eq!(
        f.store
            .lookup(lookup(
                "key-a",
                NativeObjectKind::File,
                &files.ordinary.raw_id
            ))
            .await
            .unwrap()
            .record
            .unwrap()
            .record_id,
        files.ordinary.record_id
    );
    assert_eq!(
        f.store
            .lookup(lookup(
                "key-a",
                NativeObjectKind::File,
                &files.result.raw_id
            ))
            .await
            .unwrap_err(),
        Error::NotFound
    );
    let result = f
        .store
        .lookup(contextual_lookup(files.contextual.as_ref().unwrap()))
        .await;
    assert_eq!(
        (result.map(|reply| reply.state), durable_facts(&f).await),
        (Err(Error::NotFound), before),
        "File 自身 entries 为空不能释放已撤销的第二模型结果，拒绝也不改历史计费事实"
    );
}

#[tokio::test]
async fn batch_result_files_keep_the_completed_parents_tombstone_boundary() {
    let f = Fixture::new().await;
    let files = completed_batch_files(&f, false).await;
    healthy_and_foreign_controls(&f, &files).await;
    assert_eq!(
        f.store
            .tombstone(TombstoneRequest {
                api_key_id: stable_resource_id("apikeys", "key-a"),
                record_id: files.batch.record_id,
                version: files.batch.version,
            })
            .await
            .unwrap()
            .state,
        "deleted"
    );
    let before = durable_facts(&f).await;
    assert_eq!(
        f.store
            .lookup(lookup(
                "key-a",
                NativeObjectKind::MessageBatch,
                &files.batch.raw_id,
            ))
            .await
            .unwrap_err(),
        Error::NotFound
    );
    let reopened = f.reopen().await;
    assert_eq!(
        reopened
            .lookup(lookup(
                "key-a",
                NativeObjectKind::File,
                &files.ordinary.raw_id
            ))
            .await
            .unwrap()
            .record
            .unwrap()
            .record_id,
        files.ordinary.record_id
    );
    for file in std::iter::once(&files.result).chain(files.contextual.iter()) {
        assert_eq!(
            reopened
                .lookup(lookup("key-a", NativeObjectKind::File, &file.raw_id))
                .await
                .unwrap_err(),
            Error::NotFound,
            "只保留正式 Batch 父的一跳存续范围"
        );
    }
    assert_eq!(durable_facts(&f).await, before);
}
