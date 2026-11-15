//! 从公开归属契约设计的真实文件数据库回归；不访问提供商，也不替代完整 HTTP E2E。
use super::*;
use aisix_core::resource::ResourceEntry;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;

const SECRET: [u8; 32] = [47; 32];
struct Fixture {
    dir: tempfile::TempDir,
    snapshot: SnapshotHandle<AisixSnapshot>,
    store: StandaloneNativeStore,
}

fn key(id: &str, team: &str, user: Option<&str>) -> ResourceEntry<ApiKey> {
    ResourceEntry::new(id, serde_json::from_value(json!({"key_hash":format!("hash-{id}"),"allowed_models":["*"],"team_id":team,"user_id":user})).unwrap(), 1)
}
fn snapshot() -> AisixSnapshot {
    let s = AisixSnapshot::new();
    for (id, team, user) in [
        ("key-a", "team-a", Some("user-a")),
        ("key-a2", "team-a", Some("user-a")),
        ("key-b", "team-b", Some("user-a")),
        ("key-other-member", "team-a", Some("user-b")),
        ("unowned-a", "team-a", None),
        ("unowned-b", "team-a", None),
    ] {
        s.apikeys.insert(key(id, team, user));
    }
    s.provider_keys.insert(ResourceEntry::new("provider-a", serde_json::from_value::<aisix_core::ProviderKey>(json!({"display_name":"native","provider":"openai","adapter":"openai","api_key":"test-only-secret","api_base":"https://upstream.example/v1"})).unwrap(),1));
    for id in ["model-a", "model-b"] {
        s.models.insert(ResourceEntry::new(
            id,
            serde_json::from_value::<aisix_core::Model>(
                json!({"display_name":id,"provider_key_id":"provider-a","model_name":"test-model"}),
            )
            .unwrap(),
            1,
        ));
    }
    s
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let snapshot = SnapshotHandle::new(snapshot());
        let store = StandaloneNativeStore::open(
            &dir.path().join("authority.sqlite3"),
            SECRET,
            snapshot.clone(),
        )
        .await
        .unwrap();
        Self {
            dir,
            snapshot,
            store,
        }
    }
    async fn reopen(&self) -> StandaloneNativeStore {
        StandaloneNativeStore::open(
            &self.dir.path().join("authority.sqlite3"),
            SECRET,
            self.snapshot.clone(),
        )
        .await
        .unwrap()
    }
    fn begin(&self, caller: &str, kind: NativeObjectKind, asynchronous: bool) -> BeginRequest {
        let s = self.snapshot.load();
        let provider = s.provider_keys.get_by_id("provider-a").unwrap();
        BeginRequest {
            usage_context: None,
            upstream_input_text: None,
            input_file_id: None,
            operation_id: Uuid::new_v4(),
            api_key_id: stable_resource_id("apikeys", caller),
            protocol: NativeProtocol::Openai,
            kind,
            origin: OriginBinding {
                surface: Some(native_origin_surface(NativeProtocol::Openai, kind)),
                model_id: stable_resource_id("models", "model-a"),
                provider_key_id: stable_resource_id("provider_keys", "provider-a"),
                boundary: configured_native_origin(
                    &SECRET,
                    stable_resource_id("provider_keys", "provider-a"),
                    &provider.value,
                    NativeProtocol::Openai,
                    kind,
                )
                .unwrap(),
            },
            parent_id: None,
            requested_model_id: stable_resource_id("models", "model-a"),
            requested_model: "model-a".into(),
            upstream_model: "test-model".into(),
            upstream_provider: "openai".into(),
            operation: kind.as_str().into(),
            request_id: Uuid::new_v4().to_string(),
            asynchronous,
            requested_service_tier: None,
            entries: vec![],
            observation_headers: Default::default(),
        }
    }
    async fn create(
        &self,
        caller: &str,
        kind: NativeObjectKind,
        raw: &str,
        asynchronous: bool,
    ) -> NativeRecord {
        let begin = self.begin(caller, kind, asynchronous);
        let pending = self.store.begin(begin.clone()).await.unwrap();
        assert_eq!(pending.state, "pending_submission");
        assert_eq!(pending.operation_id, Some(begin.operation_id));
        assert_eq!(pending.origin, Some(begin.origin.clone()));
        self.store
            .bind(BindRequest {
                existing_record_id: None,
                objects: vec![],
                operation_id: begin.operation_id,
                api_key_id: begin.api_key_id,
                raw_id: raw.into(),
                provider_created_at: None,
                provider_expires_at: None,
            })
            .await
            .unwrap()
            .record
            .unwrap()
    }
}
fn lookup(caller: &str, kind: NativeObjectKind, raw: &str) -> LookupRequest {
    LookupRequest {
        api_key_id: stable_resource_id("apikeys", caller),
        consumer_family: None,
        locator: ObjectLocator {
            protocol: NativeProtocol::Openai,
            kind,
            raw_id: raw.into(),
            origin: None,
            parent_id: None,
        },
    }
}
fn reserve(
    parent: &NativeRecord,
    child: &NativeRecord,
    action: RelationAction,
) -> ReserveRelationsRequest {
    ReserveRelationsRequest {
        api_key_id: stable_resource_id("apikeys", "key-a"),
        operation_id: Uuid::new_v4(),
        parent_record_id: parent.record_id,
        expected_parent_version: parent.version,
        relation: NativeRelation::VectorStoreFile,
        action,
        child_record_ids: vec![child.record_id],
        references: vec![],
    }
}
fn commit(r: &ReserveRelationsRequest) -> CommitRelationsRequest {
    CommitRelationsRequest {
        api_key_id: r.api_key_id,
        operation_id: r.operation_id,
        objects: vec![],
    }
}
fn relation(parent: &NativeRecord, child: &NativeRecord) -> LookupRelationsRequest {
    LookupRelationsRequest {
        api_key_id: stable_resource_id("apikeys", "key-a"),
        parent_record_id: parent.record_id,
        relation: NativeRelation::VectorStoreFile,
        raw_ids: vec![child.raw_id.clone()],
        consumer_family: None,
    }
}

#[tokio::test]
async fn raw_id_and_record_survive_close_reopen_and_delete() {
    let f = Fixture::new().await;
    let record = f
        .create(
            "key-a",
            NativeObjectKind::Conversation,
            "conv_original-id",
            false,
        )
        .await;
    f.store.close().await;
    let reopened = f.reopen().await;
    let fetched = reopened
        .lookup(lookup(
            "key-a2",
            NativeObjectKind::Conversation,
            "conv_original-id",
        ))
        .await
        .unwrap()
        .record
        .unwrap();
    assert_eq!(fetched.raw_id, "conv_original-id");
    assert_eq!(fetched.record_id, record.record_id);
    let owned = reopened
        .owned(OwnedRequest {
            api_key_id: stable_resource_id("apikeys", "key-a2"),
            protocol: NativeProtocol::Openai,
            kind: NativeObjectKind::Conversation,
            origin: fetched.origin.clone().unwrap(),
            parent_id: None,
            raw_ids: vec!["conv_foreign".into(), "conv_original-id".into()],
        })
        .await
        .unwrap();
    assert_eq!(owned.records.len(), 1);
    assert_eq!(owned.records[0].record_id, record.record_id);
    reopened
        .tombstone(TombstoneRequest {
            api_key_id: stable_resource_id("apikeys", "key-a2"),
            record_id: record.record_id,
            version: fetched.version,
        })
        .await
        .unwrap();
    assert_eq!(
        reopened
            .lookup(lookup(
                "key-a",
                NativeObjectKind::Conversation,
                "conv_original-id"
            ))
            .await
            .unwrap_err(),
        Error::NotFound
    );
}

#[tokio::test]
async fn tenant_member_and_ownerless_key_boundaries_are_distinct() {
    let f = Fixture::new().await;
    f.create("key-a", NativeObjectKind::File, "file-private", false)
        .await;
    assert!(f
        .store
        .lookup(lookup("key-a2", NativeObjectKind::File, "file-private"))
        .await
        .is_ok());
    for key in ["key-b", "key-other-member", "unowned-a"] {
        assert_eq!(
            f.store
                .lookup(lookup(key, NativeObjectKind::File, "file-private"))
                .await
                .unwrap_err(),
            Error::NotFound
        );
    }
    f.create("unowned-a", NativeObjectKind::File, "file-key-only", false)
        .await;
    assert_eq!(
        f.store
            .lookup(lookup("unowned-b", NativeObjectKind::File, "file-key-only"))
            .await
            .unwrap_err(),
        Error::NotFound
    );
}

#[tokio::test]
async fn current_revocation_model_endpoint_group_and_account_changes_fail_closed() {
    let f = Fixture::new().await;
    f.create("key-a", NativeObjectKind::File, "file-revoke", false)
        .await;
    let original = (*f.snapshot.load()).clone();
    let mutations: Vec<Box<dyn Fn(&AisixSnapshot)>> = vec![
        Box::new(|s| {
            s.apikeys.remove("key-a");
        }),
        Box::new(|s| {
            let mut k = (*s.apikeys.get_by_id("key-a").unwrap().value).clone();
            k.disabled = true;
            s.apikeys.insert(ResourceEntry::new("key-a", k, 2));
        }),
        Box::new(|s| {
            let mut k = (*s.apikeys.get_by_id("key-a").unwrap().value).clone();
            k.allowed_models.clear();
            s.apikeys.insert(ResourceEntry::new("key-a", k, 2));
        }),
        Box::new(|s| {
            let mut k = (*s.apikeys.get_by_id("key-a").unwrap().value).clone();
            k.allowed_endpoints = Some(vec![EndpointFamily::Responses]);
            s.apikeys.insert(ResourceEntry::new("key-a", k, 2));
        }),
        Box::new(|s| {
            let mut k = (*s.apikeys.get_by_id("key-a").unwrap().value).clone();
            k.allowed_provider_groups = vec!["not-granted".into()];
            s.apikeys.insert(ResourceEntry::new("key-a", k, 2));
        }),
        Box::new(|s| {
            s.models.remove("model-a");
        }),
        Box::new(|s| {
            let mut p = (*s.provider_keys.get_by_id("provider-a").unwrap().value).clone();
            p.api_key = "rotated".into();
            s.provider_keys
                .insert(ResourceEntry::new("provider-a", p, 2));
        }),
    ];
    for change in mutations {
        let s = original.clone();
        change(&s);
        f.snapshot.store(s);
        assert_eq!(
            f.store
                .lookup(lookup("key-a", NativeObjectKind::File, "file-revoke"))
                .await
                .unwrap_err(),
            Error::NotFound
        );
    }
    f.snapshot.store(original);
    assert!(f
        .store
        .lookup(lookup("key-a", NativeObjectKind::File, "file-revoke"))
        .await
        .is_ok());
}

#[tokio::test]
async fn same_raw_id_at_different_boundaries_never_overwrites_and_requires_selector() {
    let f = Fixture::new().await;
    let old = f
        .create("key-a", NativeObjectKind::File, "file-same", false)
        .await;
    let s = (*f.snapshot.load()).clone();
    let mut p = (*s.provider_keys.get_by_id("provider-a").unwrap().value).clone();
    p.api_key = "new-account".into();
    s.provider_keys
        .insert(ResourceEntry::new("provider-a", p, 2));
    f.snapshot.store(s);
    let new = f
        .create("key-a", NativeObjectKind::File, "file-same", false)
        .await;
    assert_ne!(old.record_id, new.record_id);
    assert_ne!(old.origin, new.origin);
    let mut req = lookup("key-a", NativeObjectKind::File, "file-same");
    req.locator.origin = old.origin;
    assert_eq!(f.store.lookup(req).await.unwrap_err(), Error::NotFound);
    assert_eq!(
        f.store
            .lookup(lookup("key-a", NativeObjectKind::File, "file-same"))
            .await
            .unwrap()
            .record
            .unwrap()
            .record_id,
        new.record_id
    );
}

#[tokio::test]
async fn two_stores_share_one_file_detach_preserves_other_relation_and_root_file() {
    let f = Fixture::new().await;
    let file = f
        .create("key-a", NativeObjectKind::File, "file-shared", false)
        .await;
    let a = f
        .create("key-a", NativeObjectKind::VectorStore, "vs-a", false)
        .await;
    let b = f
        .create("key-a", NativeObjectKind::VectorStore, "vs-b", false)
        .await;
    let ra = reserve(&a, &file, RelationAction::Attach);
    let rb = reserve(&b, &file, RelationAction::Attach);
    let reserved = f.store.reserve_relations(ra.clone()).await.unwrap();
    assert_eq!(reserved.state, "reserved");
    assert_eq!(reserved.record.unwrap().record_id, a.record_id);
    f.store.commit_relations(commit(&ra)).await.unwrap();
    f.store.reserve_relations(rb.clone()).await.unwrap();
    f.store.commit_relations(commit(&rb)).await.unwrap();
    let a = f
        .store
        .lookup(lookup("key-a", NativeObjectKind::VectorStore, "vs-a"))
        .await
        .unwrap()
        .record
        .unwrap();
    let detach = reserve(&a, &file, RelationAction::Detach);
    f.store.reserve_relations(detach.clone()).await.unwrap();
    f.store.commit_relations(commit(&detach)).await.unwrap();
    assert_eq!(
        f.store
            .lookup_relations(relation(&a, &file))
            .await
            .unwrap_err(),
        Error::NotFound
    );
    assert_eq!(
        f.store
            .lookup_relations(relation(&b, &file))
            .await
            .unwrap()
            .records
            .len(),
        1
    );
    assert!(f
        .store
        .lookup(lookup("key-a2", NativeObjectKind::File, "file-shared"))
        .await
        .is_ok());
}

#[tokio::test]
async fn uncertain_mutation_blocks_new_process_and_stale_discovery_cannot_revive_detached_member() {
    let f = Fixture::new().await;
    let file = f
        .create("key-a", NativeObjectKind::File, "file-cas", false)
        .await;
    let parent = f
        .create("key-a", NativeObjectKind::VectorStore, "vs-cas", false)
        .await;
    let attach = reserve(&parent, &file, RelationAction::Attach);
    f.store.reserve_relations(attach.clone()).await.unwrap();
    f.store
        .resolve_reservation(ResolveReservationRequest {
            api_key_id: attach.api_key_id,
            operation_id: attach.operation_id,
            upstream_rejected: false,
        })
        .await
        .unwrap();
    let second = f.reopen().await;
    assert_eq!(
        second
            .reserve_relations(reserve(&parent, &file, RelationAction::Attach))
            .await
            .unwrap_err(),
        Error::Conflict
    );
    assert_eq!(
        second
            .tombstone(TombstoneRequest {
                api_key_id: attach.api_key_id,
                record_id: parent.record_id,
                version: parent.version
            })
            .await
            .unwrap_err(),
        Error::Conflict
    );
    second.commit_relations(commit(&attach)).await.unwrap();
    let parent = second
        .lookup(lookup("key-a", NativeObjectKind::VectorStore, "vs-cas"))
        .await
        .unwrap()
        .record
        .unwrap();
    let discovery = reserve(&parent, &file, RelationAction::Discover);
    second.reserve_relations(discovery.clone()).await.unwrap();
    let detach = reserve(&parent, &file, RelationAction::Detach);
    second.reserve_relations(detach.clone()).await.unwrap();
    second.commit_relations(commit(&detach)).await.unwrap();
    assert_eq!(
        f.store
            .commit_relations(commit(&discovery))
            .await
            .unwrap_err(),
        Error::Conflict
    );
    assert_eq!(
        f.store
            .lookup_relations(relation(&parent, &file))
            .await
            .unwrap_err(),
        Error::NotFound
    );
    let parent = second
        .lookup(lookup("key-a", NativeObjectKind::VectorStore, "vs-cas"))
        .await
        .unwrap()
        .record
        .unwrap();
    let attach2 = reserve(&parent, &file, RelationAction::Attach);
    second.reserve_relations(attach2.clone()).await.unwrap();
    let linked = second.commit_relations(commit(&attach2)).await.unwrap();
    assert_eq!(linked.memberships[0].generation, 2);
}

#[tokio::test]
async fn accepted_bind_survives_revocation_but_unknown_submission_never_claims_or_resends() {
    let f = Fixture::new().await;
    let begin = f.begin("key-a", NativeObjectKind::Response, true);
    f.store.begin(begin.clone()).await.unwrap();
    let reopened = f.reopen().await;
    let pending = reopened
        .claim(ClaimRequest {
            limit: 10,
            record_id: None,
            protocol: None,
            kind: None,
        })
        .await
        .unwrap();
    assert!(pending.records.is_empty());
    let s = (*f.snapshot.load()).clone();
    s.apikeys.remove("key-a");
    f.snapshot.store(s);
    let bound = reopened
        .bind(BindRequest {
            existing_record_id: None,
            objects: vec![],
            operation_id: begin.operation_id,
            api_key_id: begin.api_key_id,
            raw_id: "resp-accepted".into(),
            provider_created_at: None,
            provider_expires_at: None,
        })
        .await
        .unwrap();
    assert_eq!(bound.record.unwrap().raw_id, "resp-accepted");
    assert_eq!(
        reopened
            .lookup(lookup("key-a", NativeObjectKind::Response, "resp-accepted"))
            .await
            .unwrap_err(),
        Error::NotFound
    );
    assert_eq!(
        reopened
            .claim(ClaimRequest {
                limit: 10,
                record_id: None,
                protocol: None,
                kind: None
            })
            .await
            .unwrap()
            .records
            .len(),
        1
    );
}

#[tokio::test]
async fn independent_process_leases_are_exclusive_and_stale_generation_is_rejected() {
    let f = Fixture::new().await;
    let record = f
        .create("key-a", NativeObjectKind::Response, "resp-lease", true)
        .await;
    let second = f.reopen().await;
    let claim = ClaimRequest {
        limit: 1,
        record_id: Some(record.record_id),
        protocol: None,
        kind: None,
    };
    let (a, b) = tokio::join!(f.store.claim(claim.clone()), second.claim(claim.clone()));
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(a.records.len() + b.records.len(), 1);
    let (owner, other, record) = if a.records.len() == 1 {
        (&f.store, &second, &a.records[0])
    } else {
        (&second, &f.store, &b.records[0])
    };
    let old = LeaseToken {
        record_id: record.record_id,
        generation: record.lease_generation.unwrap(),
    };
    assert_eq!(
        other
            .renew(LeaseRequest {
                lease: old.clone(),
                poll_after_ms: 0
            })
            .await
            .unwrap_err(),
        Error::Conflict
    );
    owner
        .reschedule(LeaseRequest {
            lease: old.clone(),
            poll_after_ms: 0,
        })
        .await
        .unwrap();
    let next = other.claim(claim).await.unwrap();
    assert_eq!(next.records.len(), 1);
    assert!(next.records[0].lease_generation.unwrap() > old.generation);
    assert_eq!(
        owner
            .renew(LeaseRequest {
                lease: old,
                poll_after_ms: 0
            })
            .await
            .unwrap_err(),
        Error::Conflict
    );
}

#[tokio::test]
async fn stable_secret_is_required_and_insecure_files_are_rejected() {
    let f = Fixture::new().await;
    f.store.close().await;
    assert!(matches!(
        StandaloneNativeStore::open(
            &f.dir.path().join("authority.sqlite3"),
            [3; 32],
            f.snapshot.clone()
        )
        .await,
        Err(Error::Conflict)
    ));
    std::fs::set_permissions(
        f.dir.path().join("authority.sqlite3"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(matches!(
        StandaloneNativeStore::open(
            &f.dir.path().join("authority.sqlite3"),
            SECRET,
            f.snapshot.clone()
        )
        .await,
        Err(Error::InvalidRequest)
    ));
}

fn terminal_usage(entry: &str) -> TerminalUsage {
    serde_json::from_value(json!({"entry_id":entry,"occurred_at":Utc::now(),"status_code":200,"prompt_tokens":17,"completion_tokens":5,"cached_prompt_tokens":7,"total_tokens":22})).unwrap()
}

#[tokio::test]
async fn lease_observation_after_revocation_atomically_keeps_artifacts_and_one_terminal_identity() {
    let f = Fixture::new().await;
    let record = f
        .create("key-a", NativeObjectKind::Response, "resp-observed", true)
        .await;
    let s = (*f.snapshot.load()).clone();
    s.apikeys.remove("key-a");
    f.snapshot.store(s);
    let worker = f.reopen().await;
    let leased = worker
        .claim(ClaimRequest {
            limit: 1,
            record_id: Some(record.record_id),
            protocol: None,
            kind: None,
        })
        .await
        .unwrap()
        .records
        .remove(0);
    let lease = LeaseToken {
        record_id: record.record_id,
        generation: leased.lease_generation.unwrap(),
    };
    let complete = CompleteRequest {
        lease: lease.clone(),
        terminal_status: "completed".into(),
        usage: vec![terminal_usage("")],
        output_file_ids: vec![],
    };
    let observed = BindObservedArtifactsRequest {
        lease: lease.clone(),
        entries: vec![ObservedArtifactEntry {
            relations: vec![],
            entry_id: String::new(),
            root: NativeChildObject {
                kind: NativeObjectKind::Response,
                raw_id: "resp-observed".into(),
            },
            objects: vec![NativeChildObject {
                kind: NativeObjectKind::ResponseItem,
                raw_id: "item-observed".into(),
            }],
        }],
        complete: Some(complete.clone()),
    };
    assert_eq!(
        worker
            .bind_observed_artifacts(observed.clone())
            .await
            .unwrap()
            .state,
        "observed"
    );
    assert_eq!(
        worker
            .bind_observed_artifacts(observed)
            .await
            .unwrap()
            .state,
        "observed"
    );
    let item = worker
        .lookup(lookup(
            "key-a2",
            NativeObjectKind::ResponseItem,
            "item-observed",
        ))
        .await
        .unwrap()
        .record
        .unwrap();
    assert_eq!(
        item.api_key_id,
        Some(stable_resource_id("apikeys", "key-a"))
    );
    let outbox = worker.pending_terminals(10).await.unwrap();
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].usage.cached_prompt_tokens, 7);
    assert_eq!(outbox[0].usage.total_tokens, 22);
    assert!(f.store.pending_terminals(10).await.unwrap().is_empty());
    worker
        .acknowledge_terminal(outbox[0].event_id)
        .await
        .unwrap();
    assert!(worker.pending_terminals(10).await.unwrap().is_empty());
    let mut changed = complete;
    changed.usage[0].prompt_tokens += 1;
    assert_eq!(worker.complete(changed).await.unwrap_err(), Error::Conflict);
}

#[tokio::test]
async fn batch_entries_have_independent_response_roots_and_conflicting_replay_is_atomic() {
    let f = Fixture::new().await;
    let mut begin = f.begin("key-a", NativeObjectKind::MessageBatch, true);
    begin.entries = ["one", "two"]
        .into_iter()
        .map(|id| NativeEntry {
            usage_context: None,
            upstream_input_text: None,
            endpoint: "/v1/responses".into(),
            entry_id: id.into(),
            requested_model: "model-a".into(),
            requested_service_tier: None,
            requested_model_id: begin.requested_model_id,
            model_id: begin.origin.model_id,
            upstream_model: "test-model".into(),
            upstream_provider: "openai".into(),
        })
        .collect();
    f.store.begin(begin.clone()).await.unwrap();
    let record = f
        .store
        .bind(BindRequest {
            existing_record_id: None,
            objects: vec![],
            operation_id: begin.operation_id,
            api_key_id: begin.api_key_id,
            raw_id: "batch-native".into(),
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
            record_id: Some(record.record_id),
            protocol: None,
            kind: None,
        })
        .await
        .unwrap()
        .records
        .remove(0);
    let lease = LeaseToken {
        record_id: record.record_id,
        generation: leased.lease_generation.unwrap(),
    };
    let entries = [
        ("one", "resp-one", "item-one"),
        ("two", "resp-two", "item-two"),
    ]
    .into_iter()
    .map(|(entry, root, item)| ObservedArtifactEntry {
        relations: vec![],
        entry_id: entry.into(),
        root: NativeChildObject {
            kind: NativeObjectKind::Response,
            raw_id: root.into(),
        },
        objects: vec![NativeChildObject {
            kind: NativeObjectKind::ResponseItem,
            raw_id: item.into(),
        }],
    })
    .collect::<Vec<_>>();
    let reply = f
        .store
        .bind_observed_artifacts(BindObservedArtifactsRequest {
            lease: lease.clone(),
            entries: entries.clone(),
            complete: None,
        })
        .await
        .unwrap();
    assert_eq!(reply.records.len(), 4);
    let one = f
        .store
        .lookup(lookup("key-a2", NativeObjectKind::Response, "resp-one"))
        .await
        .unwrap()
        .record
        .unwrap();
    let two = f
        .store
        .lookup(lookup("key-a2", NativeObjectKind::Response, "resp-two"))
        .await
        .unwrap()
        .record
        .unwrap();
    assert_ne!(one.operation_id, two.operation_id);
    let item = f
        .store
        .lookup(lookup("key-a2", NativeObjectKind::ResponseItem, "item-two"))
        .await
        .unwrap()
        .record
        .unwrap();
    assert_eq!(item.parent_id, Some(two.record_id));
    let mut conflicting = entries;
    conflicting[0].root.raw_id = "resp-conflict".into();
    conflicting[1].objects.push(NativeChildObject {
        kind: NativeObjectKind::ResponseItem,
        raw_id: "item-must-rollback".into(),
    });
    assert_eq!(
        f.store
            .bind_observed_artifacts(BindObservedArtifactsRequest {
                lease,
                entries: conflicting,
                complete: None
            })
            .await
            .unwrap_err(),
        Error::Conflict
    );
    assert_eq!(
        f.store
            .lookup(lookup(
                "key-a",
                NativeObjectKind::ResponseItem,
                "item-must-rollback"
            ))
            .await
            .unwrap_err(),
        Error::NotFound
    );
}

#[tokio::test]
async fn two_phase_delete_fences_attach_before_upstream_and_survives_revocation() {
    let f = Fixture::new().await;
    let parent = f
        .create("key-a", NativeObjectKind::VectorStore, "vs-delete", false)
        .await;
    let file = f
        .create("key-a", NativeObjectKind::File, "file-delete", false)
        .await;
    let request = ObjectDeletionRequest {
        api_key_id: stable_resource_id("apikeys", "key-a"),
        operation_id: Uuid::new_v4(),
        record_id: parent.record_id,
        version: parent.version,
    };
    f.store.reserve_delete(request.clone()).await.unwrap();
    let second = f.reopen().await;
    assert_eq!(
        second
            .reserve_relations(reserve(&parent, &file, RelationAction::Attach))
            .await
            .unwrap_err(),
        Error::Conflict
    );
    let s = (*f.snapshot.load()).clone();
    s.apikeys.remove("key-a");
    f.snapshot.store(s);
    second.commit_delete(request).await.unwrap();
    assert_eq!(
        second
            .lookup(lookup("key-a2", NativeObjectKind::VectorStore, "vs-delete"))
            .await
            .unwrap_err(),
        Error::NotFound
    );
    assert!(second
        .lookup(lookup("key-a2", NativeObjectKind::File, "file-delete"))
        .await
        .is_ok());
}

#[tokio::test]
async fn subprocess_claim_worker() {
    let Some(path) = std::env::var_os("AISIX_STANDALONE_TEST_DB") else {
        return;
    };
    let store =
        StandaloneNativeStore::open(Path::new(&path), SECRET, SnapshotHandle::new(snapshot()))
            .await
            .unwrap();
    let record = Uuid::parse_str(&std::env::var("AISIX_STANDALONE_TEST_RECORD").unwrap()).unwrap();
    let reply = store
        .claim(ClaimRequest {
            limit: 1,
            record_id: Some(record),
            protocol: None,
            kind: None,
        })
        .await
        .unwrap();
    println!("NATIVE_CLAIM={}", reply.records.len());
}

#[tokio::test]
async fn separate_os_processes_exclusively_claim_the_same_durable_job() {
    let f = Fixture::new().await;
    let record = f
        .create("key-a", NativeObjectKind::Response, "resp-process", true)
        .await;
    f.store.close().await;
    let spawn = || {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_api::standalone_store::tests::subprocess_claim_worker",
                "--nocapture",
            ])
            .env(
                "AISIX_STANDALONE_TEST_DB",
                f.dir.path().join("authority.sqlite3"),
            )
            .env("AISIX_STANDALONE_TEST_RECORD", record.record_id.to_string())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = spawn();
    let second = spawn();
    let mut claims = 0;
    for child in [first, second] {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let observed = stdout
            .lines()
            .find_map(|line| line.strip_prefix("NATIVE_CLAIM="))
            .expect("child must execute the exact worker test");
        claims += observed.parse::<usize>().unwrap();
    }
    assert_eq!(claims, 1);
}

#[tokio::test]
async fn reported_zero_training_quantity_remains_known_unpriced_and_never_becomes_a_zero_token_bill(
) {
    let f = Fixture::new().await;
    let record = f
        .create("key-a", NativeObjectKind::FineTuningJob, "ftjob-zero", true)
        .await;
    let leased = f
        .store
        .claim(ClaimRequest {
            limit: 1,
            record_id: Some(record.record_id),
            protocol: None,
            kind: None,
        })
        .await
        .unwrap()
        .records
        .remove(0);
    let request:TypedCompleteRequest=serde_json::from_value(json!({"target":{"target":"object","lease":{"record_id":record.record_id,"generation":leased.lease_generation.unwrap()}},"terminal_status":"succeeded","usage":[{"entry_id":"","upstream_model":"test-model","occurred_at":Utc::now(),"meter":{"kind":"training_tokens","trained_tokens":0}}]})).unwrap();
    let result = f.store.complete_typed(request).await.unwrap();
    assert_eq!(result.state, "reconciliation");
    assert_eq!(
        result.metering_state.as_deref(),
        Some("awaiting_reconciliation")
    );
    assert_eq!(
        result.reconciliation_reason.as_deref(),
        Some("unpriced_meter")
    );
    let fetched = f
        .store
        .lookup(lookup(
            "key-a2",
            NativeObjectKind::FineTuningJob,
            "ftjob-zero",
        ))
        .await
        .unwrap()
        .record
        .unwrap();
    assert!(fetched.completed);
    assert_eq!(
        fetched.metering_state.as_deref(),
        Some("awaiting_reconciliation")
    );
    assert!(f.store.pending_terminals(10).await.unwrap().is_empty());
}

#[tokio::test]
async fn wildcard_requested_name_grant_is_concrete_and_rechecked_against_current_pattern() {
    let f = Fixture::new().await;
    let s = (*f.snapshot.load()).clone();
    let mut model = (*s.models.get_by_id("model-a").unwrap().value).clone();
    model.display_name = "openai/*".into();
    s.models.insert(ResourceEntry::new("model-a", model, 2));
    let mut caller = (*s.apikeys.get_by_id("key-a").unwrap().value).clone();
    caller.allowed_models = vec!["openai/requested".into()];
    s.apikeys.insert(ResourceEntry::new("key-a", caller, 2));
    f.snapshot.store(s);
    let mut begin = f.begin("key-a", NativeObjectKind::File, false);
    begin.requested_model = "openai/requested".into();
    f.store.begin(begin.clone()).await.unwrap();
    f.store
        .bind(BindRequest {
            existing_record_id: None,
            objects: vec![],
            operation_id: begin.operation_id,
            api_key_id: begin.api_key_id,
            raw_id: "file-wildcard".into(),
            provider_created_at: None,
            provider_expires_at: None,
        })
        .await
        .unwrap();
    assert!(f
        .store
        .lookup(lookup("key-a", NativeObjectKind::File, "file-wildcard"))
        .await
        .is_ok());
    let s = (*f.snapshot.load()).clone();
    let mut model = (*s.models.get_by_id("model-a").unwrap().value).clone();
    model.display_name = "other/*".into();
    s.models.insert(ResourceEntry::new("model-a", model, 3));
    f.snapshot.store(s);
    assert_eq!(
        f.store
            .lookup(lookup("key-a", NativeObjectKind::File, "file-wildcard"))
            .await
            .unwrap_err(),
        Error::NotFound
    );
}

#[tokio::test]
async fn eval_run_token_receipt_preserves_reported_cache_and_total_in_the_durable_outbox() {
    let f = Fixture::new().await;
    let record = f
        .create("key-a", NativeObjectKind::EvalRun, "evalrun-metered", true)
        .await;
    let leased = f
        .store
        .claim(ClaimRequest {
            limit: 1,
            record_id: Some(record.record_id),
            protocol: None,
            kind: None,
        })
        .await
        .unwrap()
        .records
        .remove(0);
    let usage = terminal_usage("");
    let request: TypedCompleteRequest = serde_json::from_value(json!({
        "target": {"target":"object","lease":{"record_id":record.record_id,"generation":leased.lease_generation.unwrap()}},
        "terminal_status":"completed",
        "usage":[{"entry_id":"","upstream_model":"test-model","occurred_at":Utc::now(),"meter":{"kind":"tokens","usage":usage}}]
    })).unwrap();
    let completed = f.store.complete_typed(request).await.unwrap();
    assert_eq!(completed.state, "completed");
    assert_eq!(completed.metering_state.as_deref(), Some("outbox_pending"));
    f.store.close().await;
    let reopened = f.reopen().await;
    let terminals = reopened.pending_terminals(10).await.unwrap();
    assert_eq!(terminals.len(), 1);
    assert_eq!(
        terminals[0].usage.cached_prompt_tokens,
        usage.cached_prompt_tokens
    );
    assert_eq!(terminals[0].usage.total_tokens, usage.total_tokens);
    assert_eq!(terminals[0].usage.prompt_tokens, usage.prompt_tokens);
}

#[tokio::test]
async fn known_typed_measurement_replays_but_cannot_be_rewritten_after_reopen() {
    let f = Fixture::new().await;
    let record = f
        .create(
            "key-a",
            NativeObjectKind::FineTuningJob,
            "ftjob-immutable-meter",
            true,
        )
        .await;
    let leased = f
        .store
        .claim(ClaimRequest {
            limit: 1,
            record_id: Some(record.record_id),
            protocol: None,
            kind: None,
        })
        .await
        .unwrap()
        .records
        .remove(0);
    let request: TypedCompleteRequest = serde_json::from_value(json!({
        "target":{"target":"object","lease":{"record_id":record.record_id,"generation":leased.lease_generation.unwrap()}},
        "terminal_status":"succeeded",
        "usage":[{"entry_id":"","upstream_model":"test-model","occurred_at":Utc::now(),"meter":{"kind":"training_tokens","trained_tokens":0}}]
    })).unwrap();
    let first = f.store.complete_typed(request.clone()).await.unwrap();
    let replay = f.store.complete_typed(request.clone()).await.unwrap();
    assert_eq!(first.metering_state, replay.metering_state);
    let mut changed = request;
    changed.usage[0].meter = ObservedMeter::TrainingTokens {
        trained_tokens: 999,
    };
    assert_eq!(
        f.store.complete_typed(changed).await.unwrap_err(),
        Error::Conflict
    );
    f.store.close().await;
    let reopened = f.reopen().await;
    let current = reopened
        .lookup(lookup(
            "key-a",
            NativeObjectKind::FineTuningJob,
            "ftjob-immutable-meter",
        ))
        .await
        .unwrap()
        .record
        .unwrap();
    assert_eq!(
        current.metering_state.as_deref(),
        Some("awaiting_reconciliation")
    );
    assert!(reopened.pending_terminals(10).await.unwrap().is_empty());
}

#[tokio::test]
async fn canonical_terminal_survives_snapshot_removal_and_fences_a_stale_local_claim() {
    let f = Fixture::new().await;
    let record = f
        .create("key-a", NativeObjectKind::Response, "resp-canonical", true)
        .await;
    let leased = f
        .store
        .claim(ClaimRequest {
            limit: 1,
            record_id: Some(record.record_id),
            protocol: None,
            kind: None,
        })
        .await
        .unwrap()
        .records
        .remove(0);
    f.snapshot.store(AisixSnapshot::new());
    f.store
        .complete(CompleteRequest {
            lease: LeaseToken {
                record_id: record.record_id,
                generation: leased.lease_generation.unwrap(),
            },
            terminal_status: "completed".into(),
            usage: vec![terminal_usage("")],
            output_file_ids: vec![],
        })
        .await
        .unwrap();
    let first = f.store.pending_terminals(1).await.unwrap().remove(0);
    let wire: serde_json::Value = serde_json::from_str(&first.canonical_event).unwrap();
    assert_eq!(wire["event_id"], first.event_id.to_string());
    assert_eq!(wire["api_key_id"], "key-a");
    assert_eq!(wire["model_id"], "model-a");
    assert_eq!(wire["requested_model_id"], "model-a");
    assert_eq!(wire["provider_key_id"], "provider-a");
    assert_eq!(wire["cached_prompt_tokens"], 7);
    assert_eq!(wire["total_tokens"], 22);
    // 只推进本地数据库租约时钟；测试仍使用真实文件事务和真实确认入口。
    sqlx::query("UPDATE native_terminal_outbox SET delivery_until=0 WHERE event_id=?")
        .bind(first.event_id.to_string())
        .execute(&f.store.pool)
        .await
        .unwrap();
    let second = f.store.pending_terminals(1).await.unwrap().remove(0);
    assert!(second.delivery_generation > first.delivery_generation);
    assert_eq!(second.canonical_event, first.canonical_event);
    assert_eq!(
        f.store
            .acknowledge_terminal_claim(&first)
            .await
            .unwrap_err(),
        Error::Conflict
    );
    f.store.acknowledge_terminal_claim(&second).await.unwrap();
    f.store.close().await;
    assert!(f
        .reopen()
        .await
        .pending_terminals(1)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn subprocess_terminal_claim_worker() {
    use std::io::{Read, Write};
    let Some(path) = std::env::var_os("AISIX_STANDALONE_TEST_TERMINAL_DB") else {
        return;
    };
    let store = StandaloneNativeStore::open(
        Path::new(&path),
        SECRET,
        SnapshotHandle::new(AisixSnapshot::new()),
    )
    .await
    .unwrap();
    let claim = store.pending_terminals(1).await.unwrap().remove(0);
    println!("NATIVE_TERMINAL={}", serde_json::to_string(&claim).unwrap());
    std::io::stdout().flush().unwrap();
    let mut command = [0u8; 1];
    std::io::stdin().read_exact(&mut command).unwrap();
    assert_eq!(command, [b'A']);
    store.acknowledge_terminal_claim(&claim).await.unwrap();
    println!("NATIVE_TERMINAL_ACK=1");
}

#[tokio::test]
async fn separate_process_reclaims_canonical_event_and_only_its_generation_can_acknowledge() {
    use std::io::{BufRead, Write};
    let f = Fixture::new().await;
    let record = f
        .create(
            "key-a",
            NativeObjectKind::Response,
            "resp-process-outbox",
            true,
        )
        .await;
    let leased = f
        .store
        .claim(ClaimRequest {
            limit: 1,
            record_id: Some(record.record_id),
            protocol: None,
            kind: None,
        })
        .await
        .unwrap()
        .records
        .remove(0);
    f.store
        .complete(CompleteRequest {
            lease: LeaseToken {
                record_id: record.record_id,
                generation: leased.lease_generation.unwrap(),
            },
            terminal_status: "completed".into(),
            usage: vec![terminal_usage("")],
            output_file_ids: vec![],
        })
        .await
        .unwrap();
    let first = f.store.pending_terminals(1).await.unwrap().remove(0);
    sqlx::query("UPDATE native_terminal_outbox SET delivery_until=0 WHERE event_id=?")
        .bind(first.event_id.to_string())
        .execute(&f.store.pool)
        .await
        .unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "native_api::standalone_store::tests::subprocess_terminal_claim_worker",
            "--nocapture",
        ])
        .env(
            "AISIX_STANDALONE_TEST_TERMINAL_DB",
            f.dir.path().join("authority.sqlite3"),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    let claimed = loop {
        line.clear();
        assert!(
            output.read_line(&mut line).unwrap() > 0,
            "child must claim the outbox event"
        );
        if let Some(value) = line.strip_prefix("NATIVE_TERMINAL=") {
            break serde_json::from_str::<StandaloneTerminal>(value.trim()).unwrap();
        }
    };
    assert_eq!(claimed.event_id, first.event_id);
    assert_eq!(claimed.canonical_event, first.canonical_event);
    assert!(claimed.delivery_generation > first.delivery_generation);
    assert_eq!(
        f.store
            .acknowledge_terminal_claim(&first)
            .await
            .unwrap_err(),
        Error::Conflict
    );
    child.stdin.as_mut().unwrap().write_all(b"A").unwrap();
    let mut acknowledged = false;
    while output.read_line(&mut line).unwrap() > 0 {
        if line.contains("NATIVE_TERMINAL_ACK=1") {
            acknowledged = true;
        }
        line.clear();
    }
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(acknowledged);
    assert!(f.store.pending_terminals(1).await.unwrap().is_empty());
}

#[tokio::test]
async fn batch_entry_graph_cannot_borrow_another_entry_or_the_aggregate_reference_set() {
    let f = Fixture::new().await;
    let one = f
        .create(
            "key-a",
            NativeObjectKind::Conversation,
            "conv-entry-one",
            false,
        )
        .await;
    let two = f
        .create(
            "key-a",
            NativeObjectKind::Conversation,
            "conv-entry-two",
            false,
        )
        .await;
    let mut begin = f.begin("key-a", NativeObjectKind::MessageBatch, true);
    begin.entries = ["one", "two"]
        .into_iter()
        .map(|id| NativeEntry {
            usage_context: None,
            upstream_input_text: None,
            endpoint: "/v1/responses".into(),
            entry_id: id.into(),
            requested_model: "model-a".into(),
            requested_service_tier: None,
            requested_model_id: begin.requested_model_id,
            model_id: begin.origin.model_id,
            upstream_model: "test-model".into(),
            upstream_provider: "openai".into(),
        })
        .collect();
    let related:BeginRelatedRequest=serde_json::from_value(json!({
        "submission":begin,"references":[{"record_id":one.record_id,"consumer_family":"responses"},{"record_id":two.record_id,"consumer_family":"responses"}],
        "entry_references":{"one":[{"record_id":one.record_id,"consumer_family":"responses"}],"two":[{"record_id":two.record_id,"consumer_family":"responses"}]}
    })).unwrap();
    f.store.begin_related(related).await.unwrap();
    let batch = f
        .store
        .bind(BindRequest {
            existing_record_id: None,
            objects: vec![],
            operation_id: begin.operation_id,
            api_key_id: begin.api_key_id,
            raw_id: "batch-entry-refs".into(),
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
            record_id: Some(batch.record_id),
            protocol: None,
            kind: None,
        })
        .await
        .unwrap()
        .records
        .remove(0);
    let lease = LeaseToken {
        record_id: batch.record_id,
        generation: leased.lease_generation.unwrap(),
    };
    let observed = |parent| ObservedArtifactEntry {
        entry_id: "one".into(),
        root: NativeChildObject {
            kind: NativeObjectKind::Response,
            raw_id: "resp-entry-scope".into(),
        },
        objects: vec![NativeChildObject {
            kind: NativeObjectKind::ResponseItem,
            raw_id: "item-entry-scope".into(),
        }],
        relations: vec![BoundRelation {
            parent: ArtifactSelector::Record { record_id: parent },
            child: ArtifactSelector::Observed {
                kind: NativeObjectKind::ResponseItem,
                raw_id: "item-entry-scope".into(),
            },
            relation: NativeRelation::ConversationItem,
        }],
    };
    assert_eq!(
        f.store
            .bind_observed_artifacts(BindObservedArtifactsRequest {
                lease: lease.clone(),
                entries: vec![observed(two.record_id)],
                complete: None
            })
            .await
            .unwrap_err(),
        Error::InvalidRequest
    );
    let reply = f
        .store
        .bind_observed_artifacts(BindObservedArtifactsRequest {
            lease,
            entries: vec![observed(one.record_id)],
            complete: None,
        })
        .await
        .unwrap();
    assert_eq!(
        reply
            .memberships
            .iter()
            .filter(|edge| edge.relation == NativeRelation::ConversationItem)
            .count(),
        1
    );
    assert!(f
        .store
        .lookup_relations(LookupRelationsRequest {
            api_key_id: begin.api_key_id,
            parent_record_id: two.record_id,
            relation: NativeRelation::ConversationItem,
            raw_ids: vec!["item-entry-scope".into()],
            consumer_family: None
        })
        .await
        .is_err());
}

#[path = "standalone_store_graph_tests.rs"]
mod accepted_graph;

#[tokio::test]
async fn authorized_parent_relation_lookup_returns_an_empty_visible_subset_for_an_unknown_child() {
    let f = Fixture::new().await;
    let parent = f
        .create(
            "key-a",
            NativeObjectKind::ResponseContainer,
            "cntr-direct-file-read",
            false,
        )
        .await;
    let request = LookupRelationsRequest {
        api_key_id: stable_resource_id("apikeys", "key-a"),
        parent_record_id: parent.record_id,
        relation: NativeRelation::ContainerFile,
        raw_ids: vec!["cfile-not-observed-yet".into()],
        consumer_family: None,
    };
    let empty = f.store.lookup_relations(request.clone()).await.unwrap();
    assert_eq!(empty.state, "related");
    assert!(empty.records.is_empty());
    assert!(empty.memberships.is_empty());
    let mut foreign = request;
    foreign.api_key_id = stable_resource_id("apikeys", "key-b");
    assert_eq!(
        f.store.lookup_relations(foreign).await.unwrap_err(),
        Error::NotFound
    );
}

#[tokio::test]
async fn canonical_terminal_preserves_reported_modality_zero_and_never_invents_absent_counters_or_rebills(
) {
    let f = Fixture::new().await;
    let record = f
        .create(
            "key-a",
            NativeObjectKind::Response,
            "resp-modality-exact",
            true,
        )
        .await;
    let leased = f
        .store
        .claim(ClaimRequest {
            limit: 1,
            record_id: Some(record.record_id),
            protocol: None,
            kind: None,
        })
        .await
        .unwrap()
        .records
        .remove(0);
    let mut usage = terminal_usage("");
    usage.prompt_tokens = 15;
    usage.completion_tokens = 7;
    usage.prompt_audio_tokens = Some(0);
    usage.prompt_image_tokens = Some(4);
    usage.completion_audio_tokens = Some(2);
    usage.completion_image_tokens = Some(1);
    let request = CompleteRequest {
        lease: LeaseToken {
            record_id: record.record_id,
            generation: leased.lease_generation.unwrap(),
        },
        terminal_status: "completed".into(),
        usage: vec![usage],
        output_file_ids: vec![],
    };
    f.store.complete(request.clone()).await.unwrap();
    f.store.complete(request).await.unwrap();
    let claims = f.store.pending_terminals(10).await.unwrap();
    assert_eq!(claims.len(), 1);
    let event: serde_json::Value = serde_json::from_str(&claims[0].canonical_event).unwrap();
    for (field, expected) in [
        ("prompt_audio_tokens", 0),
        ("prompt_image_tokens", 4),
        ("completion_audio_tokens", 2),
        ("completion_image_tokens", 1),
        ("prompt_tokens", 15),
        ("completion_tokens", 7),
        ("total_tokens", 22),
        ("cached_prompt_tokens", 7),
    ] {
        assert_eq!(
            event.get(field),
            Some(&json!(expected)),
            "reported {field} remains exact, including an explicit zero"
        );
    }
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM native_terminal_outbox WHERE record_id=?")
            .bind(record.record_id.to_string())
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
    let absent = f
        .create(
            "key-a",
            NativeObjectKind::Response,
            "resp-modality-absent",
            true,
        )
        .await;
    let leased = f
        .store
        .claim(ClaimRequest {
            limit: 1,
            record_id: Some(absent.record_id),
            protocol: None,
            kind: None,
        })
        .await
        .unwrap()
        .records
        .remove(0);
    f.store
        .complete(CompleteRequest {
            lease: LeaseToken {
                record_id: absent.record_id,
                generation: leased.lease_generation.unwrap(),
            },
            terminal_status: "completed".into(),
            usage: vec![terminal_usage("")],
            output_file_ids: vec![],
        })
        .await
        .unwrap();
    let claims = f.store.pending_terminals(10).await.unwrap();
    assert_eq!(claims.len(), 1);
    let event: serde_json::Value = serde_json::from_str(&claims[0].canonical_event).unwrap();
    for field in [
        "prompt_audio_tokens",
        "prompt_image_tokens",
        "completion_audio_tokens",
        "completion_image_tokens",
    ] {
        assert!(
            event.get(field).is_none(),
            "unreported {field} stays absent"
        );
    }
    assert_eq!(event.get("total_tokens"), Some(&json!(22)));
}

#[path = "standalone_batch_file_context_tests.rs"]
mod batch_file_context;
#[path = "standalone_realtime_store_tests.rs"]
mod realtime;

#[path = "standalone_polling_index_tests.rs"]
mod polling_indexes;
