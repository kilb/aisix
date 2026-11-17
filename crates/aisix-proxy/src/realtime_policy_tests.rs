// 本地真实套接字复现运行中的授权收窄；使用正式 snapshot 发布，不调用外部供应商。
mod retained_policy {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    async fn peer() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let accepted = Arc::clone(&seen);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(message)) = ws.next().await {
                if let TgMessage::Text(text) = message {
                    accepted.fetch_add(1, Ordering::SeqCst);
                    let mut event: Value = serde_json::from_str(&text).unwrap();
                    if event["type"] == "session.update" {
                        event["type"] = serde_json::json!("session.updated");
                    }
                    if ws.send(TgMessage::Text(event.to_string())).await.is_err() {
                        break;
                    }
                }
            }
        });
        (addr, seen)
    }

    async fn narrowing_closes(
        groups: bool,
        beta: bool,
        intent: &str,
        translation: bool,
        asr: bool,
    ) {
        let (upstream, seen) = peer().await;
        let snap = snapshot(&format!("http://{upstream}/v1"), "openai", "openai");
        let pk = snap.provider_keys.get_by_id(PK_ID).unwrap();
        let mut configured = (*pk.value).clone();
        configured.groups = vec!["selected-account".into()];
        snap.provider_keys
            .insert(ResourceEntry::new(PK_ID, configured, 2));
        if asr {
            let model: Model = serde_json::from_value(serde_json::json!({
                "display_name":"asr-alias", "provider":"openai", "model_name":"asr-upstream", "provider_key_id":PK_ID
            })).unwrap();
            snap.models
                .insert(ResourceEntry::new("m-asr-policy", model, 1));
        }
        let (addr, state, mut usage) = serve(snap).await;
        let path = if translation {
            "/v1/realtime/translations"
        } else {
            "/v1/realtime"
        };
        let mut request = format!("ws://{addr}{path}?model=rt-model{intent}")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        if beta {
            request
                .headers_mut()
                .insert("openai-beta", "realtime=v1".parse().unwrap());
        }
        let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let clear = r#"{"type":"input_audio_buffer.clear"}"#;
        // 收窄仍包含当前族/账号时，应继续往返，而非对任意配置发布都关连接。
        for phase in 0..2 {
            if phase == 1 {
                let next = (*state.snapshot.load()).clone();
                let key = next.apikeys.get_by_id("k-1").unwrap();
                let mut changed = (*key.value).clone();
                changed.allowed_endpoints =
                    Some(vec![aisix_core::models::EndpointFamily::Realtime]);
                changed.allowed_provider_groups = vec!["selected-account".into()];
                next.apikeys.insert(ResourceEntry::new("k-1", changed, 2));
                state.snapshot.store(next);
            }
            ws.send(TgMessage::Text(clear.into())).await.unwrap();
            let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("allowed frame is relayed")
                .unwrap()
                .unwrap();
            let TgMessage::Text(text) = message else {
                panic!("allowed frame must be relayed");
            };
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap(),
                serde_json::from_str::<Value>(clear).unwrap()
            );
        }
        assert_eq!(seen.load(Ordering::SeqCst), 2);
        if asr {
            let session = if beta {
                serde_json::json!({"input_audio_transcription":{"model":"asr-alias"}})
            } else {
                serde_json::json!({"audio":{"input":{"transcription":{"model":"asr-alias"}}}})
            };
            ws.send(TgMessage::Text(
                serde_json::json!({"type":"session.update","session":session}).to_string(),
            ))
            .await
            .unwrap();
            let ack = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let TgMessage::Text(text) = ack else {
                panic!("ASR configuration must be acknowledged");
            };
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap()["type"],
                "session.updated"
            );
        }
        let accepted_before = seen.load(Ordering::SeqCst);
        let next = (*state.snapshot.load()).clone();
        let key = next.apikeys.get_by_id("k-1").unwrap();
        let mut changed = (*key.value).clone();
        if groups {
            changed.allowed_provider_groups = vec!["different-account".into()];
        } else {
            changed.allowed_endpoints =
                Some(vec![aisix_core::models::EndpointFamily::ChatCompletions]);
        }
        next.apikeys.insert(ResourceEntry::new("k-1", changed, 3));
        state.snapshot.store(next);
        // 原有客户端工作帧检查点应关闭；外部期限只使旧代码的漏检成为可失败断言。
        let revoked_frame = if asr {
            r#"{"type":"input_audio_buffer.append","audio":"AAAA"}"#
        } else {
            clear
        };
        ws.send(TgMessage::Text(revoked_frame.into()))
            .await
            .unwrap();
        let closed = tokio::time::timeout(Duration::from_secs(2), async {
            let mut error = false;
            while let Some(Ok(message)) = ws.next().await {
                match message {
                    TgMessage::Text(text) => {
                        let event: Value = serde_json::from_str(&text).unwrap();
                        error |= event["type"] == "error";
                    }
                    TgMessage::Close(frame) => {
                        return error && frame.is_some_and(|f| u16::from(f.code) == 1008)
                    }
                    _ => {}
                }
            }
            false
        })
        .await;
        assert_eq!(
            closed,
            Ok(true),
            "groups={groups} beta={beta} intent={intent} translation={translation}"
        );
        assert_eq!(
            seen.load(Ordering::SeqCst),
            accepted_before,
            "revoked frame must not reach the upstream"
        );
        let event = tokio::time::timeout(Duration::from_secs(5), usage.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.status_code, if groups { 503 } else { 403 });
        assert!(
            usage.try_recv().is_err(),
            "one terminal usage row per closed session"
        );
    }

    #[tokio::test]
    async fn endpoint_narrowing_closes_existing_ws_at_current_checkpoint() {
        for (beta, intent, translation) in [
            (false, "", false),
            (true, "", false),
            (false, "&intent=transcription", false),
            (true, "&intent=transcription", false),
            (false, "", true),
        ] {
            narrowing_closes(false, beta, intent, translation, false).await;
        }
    }

    #[tokio::test]
    async fn active_asr_obeys_endpoint_and_provider_group_narrowing() {
        for groups in [false, true] {
            for beta in [false, true] {
                narrowing_closes(groups, beta, "", false, true).await;
            }
        }
    }

    #[tokio::test]
    async fn provider_group_narrowing_closes_existing_ws_at_current_checkpoint() {
        for (beta, intent, translation) in [
            (false, "", false),
            (true, "", false),
            (false, "&intent=transcription", false),
            (true, "&intent=transcription", false),
            (false, "", true),
        ] {
            narrowing_closes(true, beta, intent, translation, false).await;
        }
    }
}
