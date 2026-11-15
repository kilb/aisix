//! 源盲已接受产物图：来源所有权、撤权后的已发生工作和迟到的集合关系分别验证。
use super::*;

async fn parents(f: &Fixture) -> (NativeRecord, NativeRecord) {
    let conversation = f
        .create(
            "key-a",
            NativeObjectKind::Conversation,
            "conv-accepted-parent",
            false,
        )
        .await;
    let container = f
        .create(
            "key-a",
            NativeObjectKind::ResponseContainer,
            "cntr-accepted-parent",
            false,
        )
        .await;
    (conversation, container)
}
fn graph_entry(
    raw: &str,
    conversation: &NativeRecord,
    container: &NativeRecord,
    accepted: bool,
) -> serde_json::Value {
    let selector = |record: &NativeRecord| {
        if accepted {
            json!({"source":"accepted","kind":record.kind,"raw_id":record.raw_id})
        } else {
            json!({"source":"record","record_id":record.record_id})
        }
    };
    json!({"entry_id":"","root":{"kind":"response","raw_id":raw},
        "objects":[{"kind":"response_item","raw_id":"item-parent-provenance"},{"kind":"container_file","raw_id":"file-parent-provenance"}],
        "relations":[{"parent":selector(conversation),"child":{"source":"observed","kind":"response_item","raw_id":"item-parent-provenance"},"relation":"conversation_item"},
            {"parent":selector(container),"child":{"source":"observed","kind":"container_file","raw_id":"file-parent-provenance"},"relation":"container_file"},
            {"parent":{"source":"root"},"child":{"source":"observed","kind":"response_item","raw_id":"item-parent-provenance"},"relation":"response_output"}]})
}
fn related(
    f: &Fixture,
    conversation: &NativeRecord,
    container: &NativeRecord,
    asynchronous: bool,
) -> BeginRelatedRequest {
    BeginRelatedRequest {
        submission: f.begin("key-a2", NativeObjectKind::Response, asynchronous),
        references: vec![
            AcceptedReference {
                record_id: conversation.record_id,
                consumer_family: EndpointFamily::Responses,
            },
            AcceptedReference {
                record_id: container.record_id,
                consumer_family: EndpointFamily::Responses,
            },
        ],
        entry_references: Default::default(),
        owner_parent_record_id: None,
        lifecycle_context: None,
    }
}
fn binding(begin: &BeginRequest, raw: &str) -> BindRequest {
    BindRequest {
        operation_id: begin.operation_id,
        api_key_id: begin.api_key_id,
        raw_id: raw.into(),
        existing_record_id: None,
        objects: vec![],
        provider_created_at: None,
        provider_expires_at: None,
    }
}

#[tokio::test]
async fn synchronous_graph_generated_children_keep_each_accepted_parent_creator() {
    let f = Fixture::new().await;
    let (conversation, container) = parents(&f).await;
    let accepted = related(&f, &conversation, &container, false);
    f.store.begin_related(accepted.clone()).await.unwrap();
    let graph = graph_entry("resp-parent-owner", &conversation, &container, false);
    let mut bind = binding(&accepted.submission, "resp-parent-owner");
    bind.objects = serde_json::from_value(graph["objects"].clone()).unwrap();
    let relations = serde_json::from_value(graph["relations"].clone()).unwrap();
    let reply = f
        .store
        .bind_related(BindRelatedRequest {
            binding: bind,
            relations,
        })
        .await
        .unwrap();
    assert_eq!(
        reply.record.as_ref().unwrap().api_key_id,
        Some(stable_resource_id("apikeys", "key-a2"))
    );
    for (kind, raw) in [
        (NativeObjectKind::ResponseItem, "item-parent-provenance"),
        (NativeObjectKind::ContainerFile, "file-parent-provenance"),
    ] {
        let child = reply
            .records
            .iter()
            .find(|record| record.kind == Some(kind) && record.raw_id == raw)
            .unwrap();
        assert_eq!(
            child.api_key_id,
            Some(stable_resource_id("apikeys", "key-a"))
        );
        let owned = f
            .store
            .lookup(lookup("key-a", kind, raw))
            .await
            .unwrap()
            .record
            .unwrap();
        assert_eq!(owned.record_id, child.record_id);
    }
}

#[tokio::test]
async fn paid_observation_retains_parent_owned_artifacts_but_never_revives_a_deleted_parent() {
    let f = Fixture::new().await;
    let (conversation, container) = parents(&f).await;
    let accepted = related(&f, &conversation, &container, true);
    f.store.begin_related(accepted.clone()).await.unwrap();
    let response = f
        .store
        .bind(binding(&accepted.submission, "resp-late-parent"))
        .await
        .unwrap()
        .record
        .unwrap();
    let leased = f
        .store
        .claim(ClaimRequest {
            limit: 1,
            record_id: Some(response.record_id),
            protocol: None,
            kind: None,
        })
        .await
        .unwrap()
        .records
        .remove(0);
    f.store
        .tombstone(TombstoneRequest {
            api_key_id: stable_resource_id("apikeys", "key-a"),
            record_id: conversation.record_id,
            version: conversation.version,
        })
        .await
        .unwrap();
    let snapshot = (*f.snapshot.load()).clone();
    let mut key = (*snapshot.apikeys.get_by_id("key-a2").unwrap().value).clone();
    key.disabled = true;
    snapshot
        .apikeys
        .insert(ResourceEntry::new("key-a2", key, 2));
    f.snapshot.store(snapshot);
    let lease = LeaseToken {
        record_id: response.record_id,
        generation: leased.lease_generation.unwrap(),
    };
    let entry = serde_json::from_value(graph_entry(
        "resp-late-parent",
        &conversation,
        &container,
        true,
    ))
    .unwrap();
    let request = BindObservedArtifactsRequest {
        lease: lease.clone(),
        entries: vec![entry],
        complete: Some(CompleteRequest {
            lease,
            terminal_status: "completed".into(),
            usage: vec![terminal_usage("")],
            output_file_ids: vec![],
        }),
    };
    let reply = f
        .store
        .bind_observed_artifacts(request.clone())
        .await
        .unwrap();
    assert_eq!(reply.memberships.len(), 2);
    assert!(reply
        .memberships
        .iter()
        .all(|edge| edge.parent_record_id != conversation.record_id));
    assert_eq!(
        reply
            .records
            .iter()
            .filter(|record| record.kind != Some(NativeObjectKind::Response))
            .count(),
        2
    );
    for child in reply
        .records
        .iter()
        .filter(|record| record.kind != Some(NativeObjectKind::Response))
    {
        assert_eq!(
            child.api_key_id,
            Some(stable_resource_id("apikeys", "key-a"))
        );
    }
    let replay = f.store.bind_observed_artifacts(request).await.unwrap();
    assert_eq!(
        serde_json::to_value(&reply).unwrap(),
        serde_json::to_value(&replay).unwrap()
    );
    assert_eq!(f.store.pending_terminals(10).await.unwrap().len(), 1);
    assert!(f
        .store
        .lookup(lookup(
            "key-a2",
            NativeObjectKind::Response,
            "resp-late-parent"
        ))
        .await
        .is_err());
}

#[tokio::test]
async fn two_paid_entries_can_append_to_the_same_accepted_parent_across_observation_calls() {
    let f = Fixture::new().await;
    let conversation = f
        .create(
            "key-a",
            NativeObjectKind::Conversation,
            "conv-two-paid-entries",
            false,
        )
        .await;
    let mut begin = f.begin("key-a2", NativeObjectKind::MessageBatch, true);
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
    let proof = json!([{"record_id":conversation.record_id,"consumer_family":"responses"}]);
    let related: BeginRelatedRequest = serde_json::from_value(
        json!({"submission":begin,"references":[],"entry_references":{"one":proof,"two":proof}}),
    )
    .unwrap();
    f.store.begin_related(related).await.unwrap();
    let batch = f
        .store
        .bind(binding(&begin, "batch-two-paid-entries"))
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
    let observed = |entry: &str| {
        serde_json::from_value(json!({"entry_id":entry,"root":{"kind":"response","raw_id":format!("resp-paid-{entry}")},
        "objects":[{"kind":"response_item","raw_id":format!("item-paid-{entry}")}],"relations":[
            {"parent":{"source":"accepted","kind":"conversation","raw_id":conversation.raw_id},"child":{"source":"observed","kind":"response_item","raw_id":format!("item-paid-{entry}")},"relation":"conversation_item"},
            {"parent":{"source":"root"},"child":{"source":"observed","kind":"response_item","raw_id":format!("item-paid-{entry}")},"relation":"response_output"}]})).unwrap()
    };
    let first = f
        .store
        .bind_observed_artifacts(BindObservedArtifactsRequest {
            lease: lease.clone(),
            entries: vec![observed("one")],
            complete: None,
        })
        .await
        .unwrap();
    assert_eq!(first.memberships.len(), 2);
    let second = f
        .store
        .bind_observed_artifacts(BindObservedArtifactsRequest {
            lease: lease.clone(),
            entries: vec![observed("two")],
            complete: Some(CompleteRequest {
                lease,
                terminal_status: "completed".into(),
                usage: vec![terminal_usage("one"), terminal_usage("two")],
                output_file_ids: vec![],
            }),
        })
        .await
        .unwrap();
    assert_eq!(second.memberships.len(), 2);
    let related = f
        .store
        .lookup_relations(LookupRelationsRequest {
            api_key_id: stable_resource_id("apikeys", "key-a"),
            parent_record_id: conversation.record_id,
            relation: NativeRelation::ConversationItem,
            raw_ids: vec!["item-paid-one".into(), "item-paid-two".into()],
            consumer_family: None,
        })
        .await
        .unwrap();
    assert_eq!(related.records.len(), 2);
    assert_eq!(related.memberships.len(), 2);
    assert!(related
        .records
        .iter()
        .all(|record| record.api_key_id == Some(stable_resource_id("apikeys", "key-a"))));
    assert_eq!(f.store.pending_terminals(10).await.unwrap().len(), 2);
}

#[tokio::test]
async fn parent_owned_artifact_keeps_its_parents_model_grant_across_a_different_response_model() {
    let f = Fixture::new().await;
    let (conversation, container) = parents(&f).await;
    let snapshot = (*f.snapshot.load()).clone();
    let mut owner = (*snapshot.apikeys.get_by_id("key-a").unwrap().value).clone();
    owner.allowed_models = vec!["model-a".into()];
    snapshot
        .apikeys
        .insert(ResourceEntry::new("key-a", owner, 2));
    f.snapshot.store(snapshot);
    let mut accepted = related(&f, &conversation, &container, false);
    accepted.submission.origin.model_id = stable_resource_id("models", "model-b");
    accepted.submission.requested_model_id = stable_resource_id("models", "model-b");
    accepted.submission.requested_model = "model-b".into();
    f.store.begin_related(accepted.clone()).await.unwrap();
    let graph = graph_entry("resp-cross-model", &conversation, &container, false);
    let mut bind = binding(&accepted.submission, "resp-cross-model");
    bind.objects = serde_json::from_value(graph["objects"].clone()).unwrap();
    let reply = f
        .store
        .bind_related(BindRelatedRequest {
            binding: bind,
            relations: serde_json::from_value(graph["relations"].clone()).unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(
        reply.record.as_ref().unwrap().requested_model_id,
        Some(stable_resource_id("models", "model-b"))
    );
    for child in &reply.records {
        assert_eq!(
            child.requested_model_id,
            Some(stable_resource_id("models", "model-a"))
        );
        assert_eq!(
            child.origin.as_ref().unwrap().model_id,
            stable_resource_id("models", "model-a")
        );
        assert_eq!(
            child.api_key_id,
            Some(stable_resource_id("apikeys", "key-a"))
        );
        assert!(f
            .store
            .lookup(lookup("key-a", child.kind.unwrap(), &child.raw_id))
            .await
            .is_ok());
    }
    assert_eq!(
        f.store
            .lookup(lookup(
                "key-a",
                NativeObjectKind::Response,
                "resp-cross-model"
            ))
            .await
            .unwrap_err(),
        Error::NotFound
    );
}
