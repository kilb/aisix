// 只调用真实会话策略与分项构造；不启动供应商或把组件合同称作真实供应商 E2E。
mod asr_billing_contract {
    use super::*;

    async fn local_session() -> crate::native_api::realtime_journal::Journal {
        let snap = snapshot("http://127.0.0.1:1/v1", "openai", "openai");
        let b: Model = serde_json::from_value(serde_json::json!({
            "display_name":"asr-alias", "provider":"openai", "model_name":"asr-upstream",
            "provider_key_id":PK_ID,
        }))
        .expect("legal local secondary model");
        snap.models.insert(ResourceEntry::new("m-asr", b, 1));
        let handle = SnapshotHandle::new(snap);
        let live = handle.load();
        let auth =
            AuthenticatedKey::new(&live, live.apikeys.get_by_id("k-1").unwrap(), None, false);
        let state = ProxyState::new(handle, Arc::new(Hub::new()), &cfg()).without_cache();
        let mut journal = crate::native_api::realtime_journal::Journal::admit(
            &state,
            &auth,
            None,
            "m-rt",
            PK_ID,
            "rt-model",
            "gpt-realtime",
            "openai",
            "actual-local-asr-component",
            &ClientContext::default(),
            None,
            aisix_core::native_realtime::RealtimePurpose::Realtime,
            Vec::new(),
            "/v1/realtime",
        )
        .await
        .expect("normal raw WS local state without a native backend")
        .expect("local ASR admission must not depend on a static origin");
        let config = serde_json::json!({"type":"session.update","session":{"audio":{"input":{
            "transcription":{"model":"asr-alias"}
        }}}})
        .to_string();
        let rewritten = journal
            .prepare_frame(&mut Frame::new(config.as_bytes()))
            .await
            .expect("actual locally granted configuration")
            .expect("the caller alias is rewritten in the actual ASR slot");
        assert_eq!(
            serde_json::from_slice::<Value>(&rewritten)
                .unwrap()
                .pointer("/session/audio/input/transcription/model"),
            Some(&serde_json::json!("asr-upstream"))
        );
        collect(
            &mut journal,
            serde_json::json!({"type":"session.updated","session":{"audio":{"input":{
                "transcription":{"model":"asr-upstream"}
            }}}}),
        );
        let item = serde_json::json!({"type":"conversation.item.create","item":{
            "id":"accepted-b-item", "type":"message", "role":"user", "content":[{"type":"input_audio","audio":"AAAA"}]
        }}).to_string();
        journal
            .prepare_frame(&mut Frame::new(item.as_bytes()))
            .await
            .expect("freeze the real accepted input item under B");
        journal
    }

    fn collect(journal: &mut crate::native_api::realtime_journal::Journal, event: Value) {
        let bytes = serde_json::to_vec(&event).unwrap();
        journal
            .collect(&Frame::new(&bytes))
            .expect("actual local fact consumer");
    }

    fn b_done(journal: &mut crate::native_api::realtime_journal::Journal) {
        collect(
            journal,
            serde_json::json!({
                "type":"conversation.item.input_audio_transcription.completed", "item_id":"accepted-b-item", "content_index":0,
                "usage":{"type":"tokens","input_tokens":5,"output_tokens":1,"total_tokens":6,
                    "input_token_details":{"cached_tokens":0,"audio_tokens":5,"text_tokens":0}}
            }),
        );
    }

    fn summary() -> UsageEvent {
        UsageEvent {
            model_id: "m-rt".into(),
            requested_model_id: "m-rt".into(),
            requested_model: "rt-model".into(),
            request_id: "same-session".into(),
            occurred_at: UsageEvent::occurred_at_now(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn unknown_primary_quantity_never_becomes_an_invented_zero_billing_row_when_asr_is_known()
    {
        for usage in [
            None,
            Some(Value::Null),
            Some(serde_json::json!({"input_tokens":0})),
            Some(serde_json::json!({"output_tokens":0})),
        ] {
            let mut journal = local_session().await;
            let mut response =
                serde_json::json!({"id":"unknown-primary-response","status":"completed"});
            if let Some(usage) = usage {
                response["usage"] = usage;
            }
            collect(
                &mut journal,
                serde_json::json!({"type":"response.done","response":response}),
            );
            b_done(&mut journal);
            let rows = journal.billing_rows(&summary());
            assert_eq!(
                rows.len(),
                1,
                "unknown primary usage is an observation, not an invented zero charge row"
            );
            assert_eq!(rows[0].1.model_id, "m-asr");
            assert_eq!(
                (
                    rows[0].1.prompt_tokens,
                    rows[0].1.completion_tokens,
                    rows[0].1.total_tokens
                ),
                (5, 1, 6)
            );
            assert_eq!(rows[0].1.modality_tokens.prompt_audio_tokens, Some(5));
            assert_eq!(rows[0].1.modality_tokens.prompt_image_tokens, None);
        }
    }

    #[tokio::test]
    async fn explicit_zero_primary_report_remains_distinct_from_unknown_quantity() {
        let mut journal = local_session().await;
        collect(
            &mut journal,
            serde_json::json!({"type":"response.done","response":{
                "id":"explicit-zero-primary","status":"completed","usage":{
                    "input_tokens":0,"output_tokens":0,"total_tokens":0,
                    "input_token_details":{"cached_tokens":0,"audio_tokens":0}
                }
            }}),
        );
        b_done(&mut journal);
        let rows = journal.billing_rows(&summary());
        assert_eq!(
            rows.len(),
            2,
            "the provider explicitly reported a zero primary fact"
        );
        let primary = rows
            .iter()
            .find(|(_, row)| row.model_id == "m-rt")
            .expect("explicit primary row");
        assert_eq!(
            (
                primary.1.prompt_tokens,
                primary.1.completion_tokens,
                primary.1.total_tokens
            ),
            (0, 0, 0)
        );
        assert_eq!(primary.1.modality_tokens.prompt_audio_tokens, Some(0));
        assert_eq!(primary.1.modality_tokens.prompt_image_tokens, None);
        let secondary = rows
            .iter()
            .find(|(_, row)| row.model_id == "m-asr")
            .expect("actual secondary row");
        assert_ne!(
            primary.1.event_id, secondary.1.event_id,
            "same-session model rows retain independent delivery identities"
        );
        assert!(!primary.1.event_id.is_empty() && !secondary.1.event_id.is_empty());
    }
}
