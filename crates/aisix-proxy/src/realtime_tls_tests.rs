// 真实本地 CA、TLS 握手和 WebSocket；不使用外部 API 或替代 connector。
mod retained_tls {
    use super::*;
    use aisix_core::models::provider_key::ProviderKeyTls;

    fn certificate_authority() -> (rcgen::Certificate, rcgen::KeyPair) {
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        (params.self_signed(&key).unwrap(), key)
    }

    fn tls_acceptor() -> (tokio_rustls::TlsAcceptor, String) {
        let (ca, ca_key) = certificate_authority();
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let cert = rcgen::CertificateParams::new(vec!["127.0.0.1".into()])
            .unwrap()
            .signed_by(&key, &ca, &ca_key)
            .unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
        (tokio_rustls::TlsAcceptor::from(Arc::new(config)), ca.pem())
    }

    async fn tls_peer() -> (std::net::SocketAddr, String) {
        let (acceptor, ca) = tls_acceptor();
        (websocket_peer(acceptor).await, ca)
    }

    async fn websocket_peer(acceptor: tokio_rustls::TlsAcceptor) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    while let Some(Ok(message)) = ws.next().await {
                        if let TgMessage::Text(text) = message {
                            if ws.send(TgMessage::Text(text)).await.is_err() {
                                break;
                            }
                        }
                    }
                });
            }
        });
        addr
    }

    // 与 WS 使用同一 CA，但独立提供真正的 HTTP 200，避免把升级失败当成证书拒绝。
    async fn https_peer(acceptor: tokio_rustls::TlsAcceptor) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let response = async {
                        let mut request = Vec::new();
                        let mut buffer = [0_u8; 1024];
                        while request.len() < 8192 {
                            let count = stream.read(&mut buffer).await?;
                            if count == 0 {
                                return Ok::<_, std::io::Error>(());
                            }
                            request.extend_from_slice(&buffer[..count]);
                            if request.windows(4).any(|part| part == b"\r\n\r\n") {
                                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await?;
                                return stream.shutdown().await;
                            }
                        }
                        Ok(())
                    };
                    let _ = tokio::time::timeout(Duration::from_secs(5), response).await;
                });
            }
        });
        addr
    }

    async fn assert_https_ok(client: &reqwest::Client, url: &str) {
        let response = client
            .get(url)
            .send()
            .await
            .expect("the HTTPS peer must answer ordinary HTTP");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "ok");
    }

    async fn assert_default_https_rejects_unknown_ca(url: &str, phase: &str) {
        let client = aisix_gateway::upstream_http::client_builder()
            .build()
            .unwrap();
        let error = client
            .get(url)
            .send()
            .await
            .expect_err("default HTTP trust must reject this private CA");
        assert!(error.is_connect(), "{phase}: {error}");
        let causes = aisix_gateway::upstream_http::error_with_causes(&error);
        assert!(
            causes.contains("UnknownIssuer"),
            "{phase}: expected certificate trust rejection, got {causes}"
        );
    }

    #[tokio::test]
    async fn global_http_trust_stays_strict_after_provider_key_overrides() {
        let (acceptor, ca) = tls_acceptor();
        let websocket = websocket_peer(acceptor.clone()).await;
        let https = https_peer(acceptor).await;
        let url = format!("https://{https}/trust-check");
        let trusted = aisix_gateway::upstream_http::client_builder()
            .add_root_certificate(reqwest::Certificate::from_pem(ca.as_bytes()).unwrap())
            .build()
            .unwrap();
        let insecure = aisix_gateway::upstream_http::client_builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        // 两个正控都必须拿到 200 和正文，排除 WS-only、端口不可达或 HTTP 解析失败。
        assert_https_ok(&trusted, &url).await;
        assert_https_ok(&insecure, &url).await;
        assert_default_https_rejects_unknown_ca(&url, "before overrides").await;
        roundtrip(
            websocket,
            Some(ProviderKeyTls {
                ca_cert: Some(ca),
                verify: true,
            }),
            true,
            "per-key CA before global trust recheck",
        )
        .await;
        assert_default_https_rejects_unknown_ca(&url, "after per-key CA").await;
        roundtrip(
            websocket,
            Some(ProviderKeyTls {
                ca_cert: None,
                verify: false,
            }),
            true,
            "per-key verify=false before global trust recheck",
        )
        .await;
        assert_default_https_rejects_unknown_ca(&url, "after per-key verify=false").await;
        assert_https_ok(&trusted, &url).await;
    }

    async fn roundtrip(
        addr: std::net::SocketAddr,
        tls: Option<ProviderKeyTls>,
        succeeds: bool,
        label: &str,
    ) {
        let snap = snapshot(&format!("https://{addr}/v1"), "openai", "openai");
        let pk = snap.provider_keys.get_by_id(PK_ID).unwrap();
        let mut configured = (*pk.value).clone();
        configured.tls = tls;
        snap.provider_keys
            .insert(ResourceEntry::new(PK_ID, configured, 2));
        let (proxy, _state, mut usage) = serve(snap).await;
        let mut ws = client(proxy).await;
        let clear = r#"{"type":"input_audio_buffer.clear"}"#;
        ws.send(TgMessage::Text(clear.into())).await.unwrap();
        let first = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if succeeds {
            assert_eq!(
                first,
                TgMessage::Text(clear.into()),
                "{label}: configured TLS must complete and relay"
            );
            ws.close(None).await.unwrap();
        } else {
            let TgMessage::Text(text) = first else {
                panic!("{label}: upstream TLS refusal must report an error");
            };
            let event: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(event["type"], "error", "{label}");
            let close = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(
                matches!(close, TgMessage::Close(Some(ref frame)) if u16::from(frame.code) == 1011),
                "{label}: {close:?}"
            );
        }
        let row = tokio::time::timeout(Duration::from_secs(5), usage.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.status_code < 400,
            succeeds,
            "{label}: {}",
            row.status_code
        );
        assert!(usage.try_recv().is_err(), "{label}: one terminal row");
    }

    #[tokio::test]
    async fn failed_dial_close_releases_usage_and_slots_before_a_silent_peer_ack() {
        let snap = snapshot("http://127.0.0.1:1/v1", "openai", "openai");
        let key = snap.apikeys.get_by_id("k-1").unwrap();
        let mut configured = (*key.value).clone();
        configured.rate_limit =
            Some(serde_json::from_value(serde_json::json!({"concurrency":1})).unwrap());
        snap.apikeys
            .insert(ResourceEntry::new("k-1", configured, 2));
        let (proxy, state, mut usage) = serve(snap).await;
        // 首连接不读 Close，也不发 ACK；失败用量仍须立即发出，额度不等两秒收尾。
        let silent = client(proxy).await;
        let row = tokio::time::timeout(Duration::from_secs(1), usage.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status_code, 502);
        let second = tokio::time::timeout(Duration::from_secs(1), client(proxy))
            .await
            .expect("the failed dial must have released the only concurrency slot");
        drop(second);
        let row = tokio::time::timeout(Duration::from_secs(1), usage.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status_code, 502);
        tokio::time::timeout(UPSTREAM_CLOSE_ACK + Duration::from_secs(1), async {
            while state.livez.in_flight() != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("a silent peer cannot retain the session beyond the existing close budget");
        assert!(
            usage.try_recv().is_err(),
            "exactly one terminal row per socket"
        );
        drop(silent);
    }

    #[tokio::test]
    async fn failed_dial_close_does_not_wait_for_an_already_broken_peer() {
        let (proxy, state, mut usage) =
            serve(snapshot("http://127.0.0.1:1/v1", "openai", "openai")).await;
        let ws = client(proxy).await;
        drop(ws);
        let row = tokio::time::timeout(Duration::from_secs(1), usage.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status_code, 502);
        tokio::time::timeout(Duration::from_secs(1), async {
            while state.livez.in_flight() != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("a broken peer should end without spending the close ACK budget");
        assert!(usage.try_recv().is_err());
    }

    #[tokio::test]
    async fn selected_provider_key_tls_verify_false_is_explicit_and_isolated() {
        let (addr, _) = tls_peer().await;
        roundtrip(
            addr,
            Some(ProviderKeyTls {
                ca_cert: None,
                verify: false,
            }),
            true,
            "explicit account opt-in",
        )
        .await;
        roundtrip(addr, None, false, "other account still verifies").await;
        roundtrip(
            addr,
            Some(ProviderKeyTls::default()),
            false,
            "default true still verifies",
        )
        .await;
    }

    #[tokio::test]
    async fn selected_provider_key_tls_custom_ca_and_verify_are_isolated() {
        let (addr, ca) = tls_peer().await;
        let wrong = certificate_authority().0.pem();
        roundtrip(
            addr,
            Some(ProviderKeyTls {
                ca_cert: Some(ca.clone()),
                verify: true,
            }),
            true,
            "correct account CA",
        )
        .await;
        roundtrip(
            addr,
            Some(ProviderKeyTls {
                ca_cert: Some(wrong),
                verify: true,
            }),
            false,
            "wrong account CA",
        )
        .await;
        roundtrip(addr, None, false, "unconfigured account").await;
        roundtrip(
            addr,
            Some(ProviderKeyTls {
                ca_cert: None,
                verify: false,
            }),
            true,
            "explicit account opt-in",
        )
        .await;
        roundtrip(
            addr,
            Some(ProviderKeyTls::default()),
            false,
            "verification still defaults true",
        )
        .await;
        roundtrip(
            addr,
            Some(ProviderKeyTls {
                ca_cert: Some(ca),
                verify: true,
            }),
            true,
            "custom CA remains independent",
        )
        .await;
        // 账户 opt-in 不能污染后续无覆盖 WS；HTTP trust 由独立 HTTPS 200 用例验证。
        roundtrip(addr, None, false, "global WS trust unchanged").await;
    }
}
