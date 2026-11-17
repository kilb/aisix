// 账号覆盖只改变该连接的信任设置；部署身份与默认配置保持原值。
mod provider_key_rustls {
    use super::*;

    #[test]
    fn default_and_empty_overrides_reuse_the_deployment_arc() {
        let global = rustls_client_config();
        for tls in [
            None,
            Some(ProviderKeyTls::default()),
            Some(ProviderKeyTls {
                ca_cert: Some(" \n".into()),
                verify: true,
            }),
        ] {
            assert!(Arc::ptr_eq(
                &global,
                &rustls_client_config_for_provider_key(tls.as_ref()).unwrap()
            ));
        }
        let insecure = ProviderKeyTls {
            ca_cert: None,
            verify: false,
        };
        let first = rustls_client_config_for_provider_key(Some(&insecure)).unwrap();
        let again = rustls_client_config_for_provider_key(Some(&insecure)).unwrap();
        assert!(
            Arc::ptr_eq(&first, &again),
            "one config per cached override"
        );
        assert!(!Arc::ptr_eq(&first, &global));
        assert!(Arc::ptr_eq(
            &global,
            &rustls_client_config_for_provider_key(None).unwrap()
        ));
    }

    #[tokio::test]
    async fn provider_ca_preserves_deployment_roots_and_real_mtls_identity() {
        let (deployment_ca, cert, key) = signed_leaf("aisix-gateway");
        let (extra_ca, _, _) = signed_leaf("private-endpoint");
        let deployment = TlsSettings {
            extra_ca_pem: Some(Arc::new(deployment_ca.as_bytes().to_vec())),
            client_identity: Some(Arc::new(ClientIdentityPem {
                cert: cert.into_bytes(),
                key: key.into_bytes(),
            })),
            verify: true,
        };
        let original = deployment.clone();
        let merged = provider_key_tls_settings(
            &deployment,
            &ProviderKeyTls {
                ca_cert: Some(extra_ca.clone()),
                verify: false,
            },
        )
        .unwrap();
        assert_eq!(
            deployment, original,
            "a key override cannot mutate deployment settings"
        );
        let pem = merged.extra_ca_pem.as_ref().unwrap();
        assert!(pem.starts_with(deployment_ca.as_bytes()));
        assert!(pem.ends_with(extra_ca.as_bytes()));
        assert!(Arc::ptr_eq(
            merged.client_identity.as_ref().unwrap(),
            deployment.client_identity.as_ref().unwrap()
        ));
        assert!(!merged.verify);
        assert!(
            mtls_peer_accepts(&merged, &deployment_ca).await,
            "the actual TLS peer must receive the deployment identity"
        );
        let mut without_identity = merged.clone();
        without_identity.client_identity = None;
        assert!(
            !mtls_peer_accepts(&without_identity, &deployment_ca).await,
            "control: the peer requires that identity"
        );
        let unchanged = provider_key_tls_settings(&deployment, &ProviderKeyTls::default()).unwrap();
        assert_eq!(unchanged, deployment);
        let already_insecure = TlsSettings {
            verify: false,
            ..deployment
        };
        assert!(
            !provider_key_tls_settings(&already_insecure, &ProviderKeyTls::default())
                .unwrap()
                .verify
        );
    }

    #[test]
    fn malformed_account_ca_fails_without_echoing_its_contents() {
        let tls = ProviderKeyTls {
            ca_cert: Some("private-invalid-ca-sentinel".into()),
            verify: false,
        };
        let error = rustls_client_config_for_provider_key(Some(&tls)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!error.to_string().contains("private-invalid-ca-sentinel"));
    }

    #[test]
    fn old_account_configs_are_bounded_without_invalidating_active_arcs() {
        let active = rustls_client_config();
        let mut cache = ProviderRustlsConfigs::new();
        for revision in 0..PROVIDER_RUSTLS_CONFIG_LIMIT + 10 {
            cache_provider_rustls_config(
                &mut cache,
                ProviderKeyTls {
                    ca_cert: Some(format!("revision-{revision}")),
                    verify: true,
                },
                Arc::clone(&active),
            );
            assert!(cache.len() <= PROVIDER_RUSTLS_CONFIG_LIMIT);
        }
        assert_eq!(cache.len(), PROVIDER_RUSTLS_CONFIG_LIMIT);
        assert_eq!(active.alpn_protocols, vec![b"http/1.1".to_vec()]);
        let key = cache.keys().next().unwrap().clone();
        let before = cache.get(&key).unwrap().clone();
        let inserted = cache_provider_rustls_config(&mut cache, key, Arc::new((*active).clone()));
        assert!(
            Arc::ptr_eq(&before, &inserted),
            "racing builders reuse the first published config"
        );
        assert_eq!(cache.len(), PROVIDER_RUSTLS_CONFIG_LIMIT);
    }
}
