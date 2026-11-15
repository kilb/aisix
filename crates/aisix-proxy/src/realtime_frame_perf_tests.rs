// 实际旧策略链的局部分配回归；不测 WS 库原有的 Text 缓冲复制或供应商网络。
mod frame_allocation {
    use super::*;
    use aisix_response_reference_test_alloc::AllocationScope;

    fn calibrated_scope() -> AllocationScope {
        let calibration = AllocationScope::start();
        let bytes = std::hint::black_box(vec![0_u8; 32 * 1024]);
        let counted = calibration.finish();
        assert!(
            counted.bytes >= bytes.len(),
            "the existing unit-test allocation instrument must be active"
        );
        drop(bytes);
        AllocationScope::start()
    }

    async fn admitted() -> (
        tempfile::TempDir,
        ProxyState,
        Prepared,
        crate::client_ip::ClientContext,
    ) {
        let directory = tempfile::tempdir().expect("private component database directory");
        let handle = SnapshotHandle::new(snapshot("http://127.0.0.1:1/v1", "openai", "openai"));
        let store = crate::native_api::standalone_store::StandaloneNativeStore::open(
            &directory.path().join("native.sqlite"),
            [67; 32],
            handle.clone(),
        )
        .await
        .expect("real protected SQLite authority");
        let state = ProxyState::new(handle.clone(), Arc::new(Hub::new()), &cfg())
            .without_cache()
            .with_native_objects(Arc::new(store));
        let live = handle.load();
        let auth = AuthenticatedKey::new(
            &live,
            live.apikeys
                .get_by_id("k-1")
                .expect("actual configured key"),
            None,
            false,
        );
        let client = crate::client_ip::ClientContext::default();
        let params = HashMap::from([("model".to_owned(), "rt-model".to_owned())]);
        let mut prepared = prepare(&state, &live, &params, &HeaderMap::new(), &client, auth)
            .await
            .expect("actual Realtime preparation without opening a provider socket");
        prepared.journal = crate::native_api::realtime_journal::Journal::admit(
            &state,
            &prepared.auth,
            prepared.voice_origin.as_ref(),
            &prepared.model_entry.id,
            &prepared.pk_id,
            &prepared.requested_model,
            &prepared.upstream_model,
            &prepared.upstream_provider,
            "frame-allocation-component",
            &client,
            None,
            aisix_core::native_realtime::RealtimePurpose::Realtime,
            prepared.model_reservation.keys(),
            "/v1/realtime",
        )
        .await
        .expect("actual persisted live admission");
        assert!(prepared.journal.is_some(), "this measures the journal path");
        (directory, state, prepared, client)
    }

    #[tokio::test]
    async fn actual_client_policy_chain_does_not_allocate_a_pcm_sized_value() {
        let (_directory, state, mut prepared, client) = admitted().await;
        let payload = format!(
            "{{\"type\":\"input_audio_buffer.append\",\"audio\":\"{}\"}}",
            "A".repeat(32 * 1024)
        );
        let meter = SessionMeter {
            endpoint: "/v1/realtime",
            state: &state,
            auth: &prepared.auth,
            model_entry: &prepared.model_entry,
            pk_id: &prepared.pk_id,
            provider_label: &prepared.provider_label,
            requested_model: &prepared.requested_model,
            client_type: "sdk",
            token_keys: prepared.reservation.keys(),
            model_token_keys: prepared.model_reservation.keys(),
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            secondary_input_tokens: 0,
            secondary_output_tokens: 0,
            secondary_cached_tokens: 0,
            cost_usd: 0.0,
            responses: 0,
        };
        // admission/文件事务/输入构造均在计数前完成；只量真正的三段生产策略调用。
        let scope = calibrated_scope();
        let mut frame = Frame::new(payload.as_bytes());
        crate::native_api::voices::validate_realtime_config(
            &state,
            &prepared.auth,
            &client,
            &frame,
            prepared.voice_origin.as_ref(),
            &prepared.requested_model,
        )
        .await
        .expect("plain PCM has no private voice reference");
        meter
            .check_tokens(&frame)
            .await
            .expect("actual request/model quota check");
        let rewritten = prepared
            .journal
            .as_mut()
            .expect("actual admitted journal")
            .prepare_frame(&mut frame)
            .await
            .expect("actual journal frame preparation");
        let allocated = scope.finish();
        assert!(rewritten.is_none(), "PCM content must remain verbatim");
        eprintln!(
            "actual_client_policy_allocations calls={} bytes={} full_value_decodes={}",
            allocated.calls,
            allocated.bytes,
            frame.decode_count()
        );
        assert!(
            allocated.bytes < payload.len() / 4,
            "the actual policy chain copied a PCM-sized payload: {} bytes",
            allocated.bytes
        );
    }

    #[tokio::test]
    async fn actual_supplier_usage_chain_does_not_allocate_an_audio_delta_sized_value() {
        let (_directory, _state, mut prepared, _client) = admitted().await;
        let payload = format!(
            "{{\"type\":\"response.output_audio.delta\",\"delta\":\"{}\"}}",
            "A".repeat(32 * 1024)
        );
        let mut usage = SessionUsage::default();
        let scope = calibrated_scope();
        let frame = Frame::new(payload.as_bytes());
        observe_usage(&mut prepared.journal, &mut usage, &frame)
            .await
            .expect("actual supplier observation path");
        let allocated = scope.finish();
        assert_eq!(usage.responses, 0, "audio delta is not a paid usage fact");
        eprintln!(
            "actual_supplier_policy_allocations calls={} bytes={} full_value_decodes={}",
            allocated.calls,
            allocated.bytes,
            frame.decode_count()
        );
        assert!(
            allocated.bytes < payload.len() / 4,
            "the actual supplier policy chain copied the audio payload: {} bytes",
            allocated.bytes
        );
    }
    #[tokio::test]
    async fn legacy_voice_byte_wrapper_keeps_plain_audio_opaque() {
        let (_directory, state, prepared, client) = admitted().await;
        crate::native_api::voices::validate_realtime_frame(
            &state,
            &prepared.auth,
            &client,
            br#"{"type":"input_audio_buffer.append","audio":"AAAAAA=="}"#,
            prepared.voice_origin.as_ref(),
            &prepared.requested_model,
        )
        .await
        .expect("legacy byte-wrapper compatibility");
    }
}
