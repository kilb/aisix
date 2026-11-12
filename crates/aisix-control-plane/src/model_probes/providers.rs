//! 精确直连目标的协议构造；只复用现有适配器，不走模型组或重试调度。

use std::sync::Arc;
use std::time::Duration;

use aisix_core::{Adapter, Model, ProviderKey};
use aisix_gateway::{Bridge, BridgeError, ChatFormat};
use aisix_provider_anthropic::AnthropicBridge;
use aisix_provider_azure_openai::AzureOpenAiBridge;
use aisix_provider_bedrock::BedrockBridge;
use aisix_provider_openai::OpenAiBridge;
use aisix_provider_vertex::VertexBridge;
use reqwest::Url;

use crate::error::ApiError;
use crate::upstream_http;

pub struct Prepared {
    pub bridge: Arc<dyn Bridge>,
    pub provider_key: ProviderKey,
}

pub enum PrepareError {
    Unsupported(String),
    Failed(ApiError),
    Bridge(BridgeError),
}

impl From<ApiError> for PrepareError {
    fn from(value: ApiError) -> Self {
        Self::Failed(value)
    }
}

impl From<BridgeError> for PrepareError {
    fn from(value: BridgeError) -> Self {
        Self::Bridge(value)
    }
}

/// 网络地址仍由适配器自身解析；模型和凭据交换分别经过相同出站策略。
pub async fn prepare(
    outbound: &Arc<upstream_http::Client>,
    model: &Model,
    request: &ChatFormat,
    pk: &ProviderKey,
    timeout: Duration,
) -> Result<Prepared, PrepareError> {
    let adapter = adapter(pk)?;
    check_probe_overrides(model, request, pk, adapter)?;
    if model.is_embedding() && adapter == Adapter::Anthropic {
        // 该适配器沿用 Bridge 的明确缺省能力，不能把未实现的接口当网络故障。
        return Err(BridgeError::UnsupportedCapability(
            aisix_gateway::BridgeCapability::Embeddings,
        )
        .into());
    }
    // 仅用于无网络的地址解析；真正发出请求的连接在预检后构建。
    let resolver_client = outbound.credential_client(&[], timeout).await?;
    let upstream_model = model
        .model_name
        .as_deref()
        .ok_or_else(|| BridgeError::InvalidUpstreamConfig("模型缺少上游模型标识".into()))?;
    let bedrock_http = adapter == Adapter::Bedrock
        && !model.is_embedding()
        && BedrockBridge::uses_reqwest_transport(request, upstream_model);
    let (model_url, credential_url) = match adapter {
        Adapter::Openai => (OpenAiBridge::outbound_url(pk, model.is_embedding())?, None),
        Adapter::Anthropic => (AnthropicBridge::outbound_url(pk)?, None),
        Adapter::AzureOpenai => AzureOpenAiBridge::with_client(resolver_client.clone())
            .outbound_urls(pk, upstream_model, model.is_embedding())?,
        Adapter::Vertex => VertexBridge::with_client(resolver_client).outbound_urls(pk)?,
        Adapter::Bedrock => {
            if BedrockBridge::uses_process_credentials(pk)? {
                return Err(PrepareError::Unsupported(
                    "该凭据来自网关运行环境，控制平面无法代表网关的 AWS 身份，请在网关侧验证"
                        .into(),
                ));
            }
            let model_url = if bedrock_http {
                BedrockBridge::http_outbound_url(pk)?
            } else {
                BedrockBridge::outbound_url(pk).await?
            };
            (model_url, None)
        }
    };
    let model_url = parsed_url(&model_url)?;
    let credential_urls = credential_url
        .as_deref()
        .map(parsed_url)
        .transpose()?
        .into_iter()
        .collect::<Vec<_>>();
    aisix_gateway::response_tap::note_url_userinfo(model_url.as_str());
    for url in &credential_urls {
        aisix_gateway::response_tap::note_url_userinfo(url.as_str());
    }
    let mut connection_key = pk.clone();
    if adapter == Adapter::Bedrock && !bedrock_http {
        // SDK 轨原本不读取模型的连接覆盖，探测保持该轨的真实连接语义。
        connection_key.tls = None;
        connection_key.resolve_addresses = None;
    }
    let model_client = outbound
        .probe_client(&model_url, &connection_key, timeout)
        .await?;
    let credential_client = if credential_urls.is_empty() {
        model_client.clone()
    } else {
        outbound
            .credential_client(&credential_urls, timeout)
            .await?
    };
    let bridge: Arc<dyn Bridge> = match adapter {
        Adapter::Openai => Arc::new(OpenAiBridge::with_client(model_client)),
        Adapter::Anthropic => Arc::new(AnthropicBridge::with_client(model_client)),
        Adapter::AzureOpenai => Arc::new(AzureOpenAiBridge::with_clients(
            model_client,
            credential_client,
        )),
        Adapter::Vertex => Arc::new(VertexBridge::with_clients(model_client, credential_client)),
        Adapter::Bedrock => Arc::new(
            BedrockBridge::with_clients(
                model_client.clone(),
                super::sdk_http::client(model_client),
            )
            .with_credentials_http(super::sdk_http::credential_client(
                Arc::clone(outbound),
                timeout,
            )),
        ),
    };
    // 网络覆盖已经应用于受策略约束的连接；避免适配器重新建池绕过地址固定。
    let mut provider_key = pk.clone();
    provider_key.tls = None;
    provider_key.resolve_addresses = None;
    Ok(Prepared {
        bridge,
        provider_key,
    })
}

/// 只拒绝会移动确切目标或破坏探测预算的覆盖规则，不修改账号或数据面行为。
fn check_probe_overrides(
    model: &Model,
    request: &ChatFormat,
    pk: &ProviderKey,
    adapter: Adapter,
) -> Result<(), PrepareError> {
    use aisix_provider_openai::overrides::{apply_default_body_fields, apply_param_renames};
    use aisix_provider_openai::reasoning::{
        apply_reasoning_token_cap, is_reasoning_model, ReasoningFamily,
    };
    use aisix_provider_vertex::VertexPublisher;

    let Some(overrides) = pk.request.as_ref() else {
        return Ok(());
    };
    let conflict = |reason: &str| {
        PrepareError::Unsupported(format!(
            "账号的请求覆盖配置{reason}，无法对确切目标执行有界小请求；本次未调用上游"
        ))
    };
    if overrides
        .param_renames
        .iter()
        .any(|(from, to)| from != to && (from == "model" || to == "model"))
    {
        return Err(conflict("会改写 model 字段"));
    }
    if model.is_embedding() {
        return Ok(());
    }
    let upstream = model.model_name.as_deref().unwrap_or_default();
    let bedrock_http =
        adapter == Adapter::Bedrock && BedrockBridge::uses_reqwest_transport(request, upstream);
    // Converse 的 SDK 构造不执行这些顶层重命名/默认值，保留其已有语义。
    if adapter == Adapter::Bedrock && !bedrock_http {
        return Ok(());
    }
    let publisher = (adapter == Adapter::Vertex)
        .then(|| VertexPublisher::from_upstream_id(upstream))
        .flatten();
    if publisher == Some(VertexPublisher::Google) {
        // maxOutputTokens 和 candidateCount 同在该容器中，且固定小请求必定写入它。
        // https://cloud.google.com/vertex-ai/generative-ai/docs/reference/rest/v1/GenerationConfig
        if overrides.param_renames.iter().any(|(from, to)| {
            from != to && (from == "generationConfig" || to == "generationConfig")
        }) {
            return Err(conflict(
                "会移动或覆盖 generationConfig 中的候选数和输出上限",
            ));
        }
        return Ok(());
    }
    let cap_fields = ["max_tokens", "max_completion_tokens"];
    for (from, to) in &overrides.param_renames {
        if from == to {
            continue;
        }
        let from_cap = cap_fields.contains(&from.as_str());
        let to_cap = cap_fields.contains(&to.as_str());
        if (from_cap || to_cap || from == "n" || to == "n") && !(from_cap && to_cap) {
            return Err(conflict("会改写固定候选数 n 或移除输出 token 上限"));
        }
    }
    let Some(cap) = request.max_tokens else {
        return Err(conflict("无法保留输出 token 上限"));
    };
    // 仅投影预算字段；使用适配器实际调用的转换函数，包含默认值回填后的优先级。
    let mut budget = serde_json::json!({"max_tokens": cap});
    let single_answer =
        adapter == Adapter::Anthropic || publisher == Some(VertexPublisher::Anthropic);
    if !single_answer {
        budget["n"] = serde_json::json!(1)
    }
    if let Some(explicit) = request.extra.get("max_completion_tokens") {
        budget["max_completion_tokens"] = explicit.clone()
    }
    let native_openai = pk.provider.trim().eq_ignore_ascii_case("openai")
        || (pk.provider.trim().is_empty()
            && pk
                .api_base
                .as_deref()
                .is_none_or(|base| base.trim().is_empty()));
    let reasoning = match adapter {
        Adapter::Openai if native_openai => is_reasoning_model(ReasoningFamily::Openai, upstream),
        Adapter::AzureOpenai => is_reasoning_model(ReasoningFamily::AzureOpenai, upstream),
        _ => false,
    };
    if reasoning {
        apply_reasoning_token_cap(&mut budget)
    }
    apply_param_renames(&mut budget, &overrides.param_renames);
    let had_max_tokens = budget.get("max_tokens").is_some();
    apply_default_body_fields(&mut budget, &overrides.default_body_fields);
    if (reasoning && !had_max_tokens) || bedrock_http {
        apply_reasoning_token_cap(&mut budget)
    }
    if !single_answer && budget.get("n").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err(conflict("会放大固定候选数 n"));
    }
    if single_answer {
        // 原生 Messages 只读取 max_tokens，同义的 OpenAI 字段不能代替有效上限。
        return if budget
            .get("max_tokens")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|value| value <= u64::from(cap))
        {
            Ok(())
        } else {
            Err(conflict("会移除或放大原生 Messages 的 max_tokens 上限"))
        };
    }
    let mut bounded = false;
    for field in cap_fields {
        if let Some(value) = budget.get(field) {
            bounded = true;
            if value.as_u64().is_none_or(|value| value > u64::from(cap)) {
                return Err(conflict("会重新填入更大或无法确认的输出 token 上限"));
            }
        }
    }
    if !bounded {
        return Err(conflict("会移除输出 token 上限"));
    }
    Ok(())
}

fn adapter(pk: &ProviderKey) -> Result<Adapter, PrepareError> {
    // 与 Hub 的两层分派一致，规范供应商优先于可选适配器字段。
    match pk.provider.as_str() {
        "openai" => Ok(Adapter::Openai),
        "anthropic" => Ok(Adapter::Anthropic),
        _ => pk
            .adapter
            .ok_or_else(|| PrepareError::Unsupported("该上游没有可用于探测的协议适配器".into())),
    }
}

fn parsed_url(raw: &str) -> Result<Url, BridgeError> {
    let url = Url::parse(raw)
        .map_err(|_| BridgeError::InvalidUpstreamConfig("上游连接地址不是合法 URL".into()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(BridgeError::InvalidUpstreamConfig(
            "上游连接地址必须使用 http 或 https".into(),
        ));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aisix_core::config::SsrfProtectionConfig;
    use serde_json::json;

    fn key(adapter: Adapter) -> ProviderKey {
        let secret = match adapter {
            Adapter::Vertex => json!({
                "project": "probe-project",
                "region": "us-central1",
                "access_token": "probe-access-token"
            })
            .to_string(),
            Adapter::Bedrock => json!({
                "auth_type": "api_key",
                "region": "us-east-1",
                "api_key": "ABSK-probe-test"
            })
            .to_string(),
            _ => "probe-key".to_string(),
        };
        serde_json::from_value(json!({
            "display_name": "probe-account",
            "provider": "custom",
            "adapter": adapter,
            "api_key": secret,
            "api_base": "http://127.0.0.1:9",
            "tls": {"verify": false}
        }))
        .expect("合法的账号配置")
    }

    fn short_request(model: &Model) -> ChatFormat {
        let mut request = ChatFormat::new(model.display_name.clone(), vec![]);
        request.max_tokens = Some(64);
        request.extra.insert("n".into(), json!(1));
        request
    }

    async fn override_preparation(
        adapter: Adapter,
        upstream: &str,
        overrides: serde_json::Value,
    ) -> Result<Prepared, PrepareError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let outbound = Arc::new(
            upstream_http::Client::new(&SsrfProtectionConfig {
                enabled: false,
                ..Default::default()
            })
            .expect("网络策略"),
        );
        let model: Model = serde_json::from_value(json!({
            "display_name": "probe-target", "model_name": upstream
        }))
        .expect("模型配置");
        let mut pk = key(adapter);
        pk.request = serde_json::from_value(overrides).expect("请求覆盖");
        let request = short_request(&model);
        prepare(&outbound, &model, &request, &pk, Duration::from_secs(1)).await
    }

    fn overridden_wire(
        mut body: serde_json::Value,
        overrides: &serde_json::Value,
    ) -> serde_json::Value {
        let request: aisix_core::RequestOverrides =
            serde_json::from_value(overrides.clone()).expect("请求覆盖");
        aisix_provider_openai::overrides::apply_param_renames(&mut body, &request.param_renames);
        aisix_provider_openai::overrides::apply_default_body_fields(
            &mut body,
            &request.default_body_fields,
        );
        body
    }

    #[tokio::test]
    async fn budget_overrides_cannot_increase_candidates_or_drop_the_cap() {
        let overrides = json!({"param_renames": {"max_tokens": "n"}});
        let wire = overridden_wire(json!({"max_tokens": 64, "n": 1}), &overrides);
        assert_eq!(wire["n"], 64, "真实重命名流水线覆盖了单候选约束");
        assert!(wire.get("max_tokens").is_none());
        assert!(
            matches!(
                override_preparation(Adapter::Openai, "probe-model", overrides).await,
                Err(PrepareError::Unsupported(_))
            ),
            "不能保留固定小预算的配置必须在请求前拒绝"
        );
    }

    #[tokio::test]
    async fn alias_defaults_cannot_restore_a_larger_native_cap() {
        let overrides = json!({
            "param_renames": {"max_tokens": "max_completion_tokens"},
            "default_body_fields": {"max_tokens": 8192}
        });
        let wire = overridden_wire(json!({"max_tokens": 64}), &overrides);
        assert_eq!(
            wire["max_tokens"], 8192,
            "默认值重新填入 Anthropic 实际读取的上限"
        );
        assert_eq!(wire["max_completion_tokens"], 64);
        assert!(matches!(
            override_preparation(Adapter::Anthropic, "claude-probe", overrides).await,
            Err(PrepareError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn a_rename_and_default_cannot_replace_the_exact_model_target() {
        let overrides = json!({
            "param_renames": {"model": "alias"},
            "default_body_fields": {"model": "another-upstream-model"}
        });
        let wire = overridden_wire(
            json!({"model": "probe-model", "max_tokens": 64, "n": 1}),
            &overrides,
        );
        assert_eq!(wire["model"], "another-upstream-model");
        assert!(matches!(
            override_preparation(Adapter::Openai, "probe-model", overrides).await,
            Err(PrepareError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn compatible_cap_aliases_and_unrelated_renames_keep_the_fixed_wire_budget() {
        for (from, to) in [
            ("max_tokens", "max_completion_tokens"),
            ("max_completion_tokens", "max_tokens"),
        ] {
            let overrides = json!({
                "param_renames": {from: to, "temperature": "top_p", "model": "model"},
                "default_body_fields": {"seed": 42}
            });
            let mut body = json!({"n": 1, "temperature": 0.2});
            body[from] = json!(64);
            let wire = overridden_wire(body, &overrides);
            assert_eq!(wire[to], 64);
            assert_eq!(wire["n"], 1);
            assert_eq!(wire["top_p"], 0.2);
            for (adapter, model) in [
                (Adapter::Openai, "probe-model"),
                (Adapter::AzureOpenai, "probe-deployment"),
                (Adapter::Vertex, "meta/llama-3.3-70b-instruct-maas"),
                (Adapter::Vertex, "mistral-large"),
                (Adapter::Vertex, "jamba-1.5-large"),
            ] {
                assert!(
                    override_preparation(adapter, model, overrides.clone())
                        .await
                        .is_ok(),
                    "目标支持的 cap 映射及其它无预算影响的规则不能被泛禁"
                );
            }
        }
        let ordinary_defaults = json!({"default_body_fields": {"max_tokens": 8192, "n": 100}});
        let wire = overridden_wire(json!({"max_tokens": 64, "n": 1}), &ordinary_defaults);
        assert_eq!(wire["max_tokens"], 64);
        assert_eq!(wire["n"], 1);
        assert!(
            override_preparation(Adapter::Openai, "probe-model", ordinary_defaults)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn native_messages_must_keep_its_own_effective_max_tokens_slot() {
        let overrides = json!({"param_renames": {"max_tokens": "max_completion_tokens"}});
        for (adapter, model) in [
            (Adapter::Anthropic, "claude-probe"),
            (Adapter::Vertex, "claude-probe"),
        ] {
            assert!(matches!(
                override_preparation(adapter, model, overrides.clone()).await,
                Err(PrepareError::Unsupported(_))
            ));
        }
    }

    #[tokio::test]
    async fn gemini_budget_container_is_kept_and_converse_ignores_inapplicable_rules() {
        let overrides = json!({
            "param_renames": {"generationConfig": "elsewhere"},
            "default_body_fields": {"generationConfig": {"maxOutputTokens": 8192, "candidateCount": 20}}
        });
        let body = json!({"generationConfig": {"maxOutputTokens": 64, "candidateCount": 1}});
        let wire = overridden_wire(body.clone(), &overrides);
        assert_eq!(wire["generationConfig"]["maxOutputTokens"], 8192);
        assert_eq!(wire["generationConfig"]["candidateCount"], 20);
        assert!(matches!(
            override_preparation(Adapter::Vertex, "gemini-2.5-flash", overrides).await,
            Err(PrepareError::Unsupported(_))
        ));
        let harmless_defaults = json!({
            "param_renames": {"temperature": "top_p"},
            "default_body_fields": {"generationConfig": {"maxOutputTokens": 8192, "candidateCount": 20}}
        });
        assert_eq!(
            overridden_wire(body, &harmless_defaults)["generationConfig"],
            json!({"maxOutputTokens": 64, "candidateCount": 1})
        );
        assert!(
            override_preparation(Adapter::Vertex, "gemini-2.5-flash", harmless_defaults)
                .await
                .is_ok()
        );
        // 真实 Converse 构造不运行顶层重命名/defaults，不能因惰性配置误报。
        assert!(override_preparation(
            Adapter::Bedrock,
            "amazon.nova-micro-v1:0",
            json!({"param_renames": {"max_tokens": "n"}})
        )
        .await
        .is_ok());
        assert!(matches!(
            override_preparation(
                Adapter::Bedrock,
                "global.openai.gpt-5.6",
                json!({"param_renames": {"max_tokens": "n"}})
            )
            .await,
            Err(PrepareError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn declared_anthropic_messages_base_is_checked_instead_of_the_unused_base() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let outbound = upstream_http::Client::new(&SsrfProtectionConfig {
            allowed_hosts: vec!["127.0.0.1:31001".into()],
            ..Default::default()
        })
        .expect("网络策略");
        let outbound = Arc::new(outbound);
        let model: Model = serde_json::from_value(json!({
            "display_name": "probe-anthropic", "model_name": "claude-probe"
        }))
        .expect("模型配置");
        let request = ChatFormat::new(model.display_name.clone(), vec![]);
        for unused_base in ["https://8.8.8.8", "http://127.0.0.1:31001"] {
            let mut pk = key(Adapter::Anthropic);
            pk.api_base = Some(unused_base.into());
            pk.apis = serde_json::from_value(json!({
                "messages": {"base": "http://userinfo:probe-credential-raw-7d41@127.0.0.1:31002"}
            }))
            .expect("声明消息端点");
            let tap = Arc::new(aisix_gateway::response_tap::ResponseHeaderTap::new(
                vec![],
                false,
                "probe-declared-url".into(),
            ));
            aisix_gateway::response_tap::scope(tap, async {
                let result = prepare(&outbound, &model, &request, &pk, Duration::from_secs(1)).await;
                assert!(matches!(result, Err(PrepareError::Failed(error)) if error.code == "upstream_unreachable"), "实际私网目标或未授权端口必须被拒绝");
                let scrubbed = aisix_gateway::upstream_error_scrub::scrub_upstream_error_text(
                    "upstream echoed probe-credential-raw-7d41",
                );
                assert!(!scrubbed.contains("probe-credential-raw-7d41"), "消息端点的userinfo也必须登记脱敏");
            })
            .await;
        }
        let mut pk = key(Adapter::Anthropic);
        pk.api_base = Some("http://169.254.169.254".into());
        pk.apis = serde_json::from_value(json!({
            "messages": {"base": "http://127.0.0.1:31001"}
        }))
        .expect("声明消息端点");
        assert!(
            prepare(&outbound, &model, &request, &pk, Duration::from_secs(1))
                .await
                .is_ok(),
            "未使用的基础地址不应替代实际目标的策略判断"
        );
    }

    #[tokio::test]
    async fn every_adapter_prepares_one_exact_target_without_a_model_call() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let outbound = upstream_http::Client::new(&SsrfProtectionConfig {
            enabled: false,
            ..Default::default()
        })
        .expect("测试网络策略");
        let outbound = Arc::new(outbound);
        let model: Model = serde_json::from_value(json!({
            "display_name": "probe-target",
            "provider": "custom",
            "provider_key_id": "target-account",
            "model_name": "probe-model"
        }))
        .expect("合法的直连模型");
        for adapter in Adapter::ALL {
            let pk = key(adapter);
            let request = ChatFormat::new(model.display_name.clone(), vec![]);
            let prepared =
                match prepare(&outbound, &model, &request, &pk, Duration::from_secs(1)).await {
                    Ok(prepared) => prepared,
                    Err(_) => panic!("适配器未准备成功：{adapter:?}"),
                };
            assert_eq!(prepared.bridge.wire_protocol(), adapter.wire_protocol());
            assert_eq!(prepared.provider_key.api_key, pk.api_key);
            assert_eq!(prepared.provider_key.api_base, pk.api_base);
            assert!(prepared.provider_key.tls.is_none(), "连接覆盖不能再次建池");
            assert!(prepared.provider_key.resolve_addresses.is_none());
        }
    }

    #[test]
    fn canonical_vendors_keep_the_hubs_specialized_dispatch_priority() {
        let mut pk = key(Adapter::Vertex);
        pk.provider = "openai".into();
        assert!(matches!(adapter(&pk), Ok(Adapter::Openai)));
        pk.provider = "anthropic".into();
        assert!(matches!(adapter(&pk), Ok(Adapter::Anthropic)));
    }

    #[tokio::test]
    async fn a_gateway_process_credential_is_not_replaced_with_the_control_planes_identity() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let outbound = upstream_http::Client::new(&Default::default()).expect("网络策略");
        let outbound = Arc::new(outbound);
        let mut pk = key(Adapter::Bedrock);
        pk.api_key = json!({
            "auth_type": "default_chain",
            "region": "us-east-1"
        })
        .to_string();
        let model: Model = serde_json::from_value(json!({
            "display_name": "probe-target",
            "model_name": "amazon.nova-micro-v1:0"
        }))
        .expect("模型配置");
        let request = ChatFormat::new(model.display_name.clone(), vec![]);
        let result = prepare(&outbound, &model, &request, &pk, Duration::from_secs(1)).await;
        assert!(
            matches!(result, Err(PrepareError::Unsupported(reason)) if reason.contains("网关的 AWS 身份"))
        );
    }

    #[tokio::test]
    async fn sdk_and_http_rails_keep_the_gateways_connection_override_semantics() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let outbound = upstream_http::Client::new(&SsrfProtectionConfig {
            enabled: false,
            ..Default::default()
        })
        .expect("网络策略");
        let outbound = Arc::new(outbound);
        let mut pk = key(Adapter::Bedrock);
        pk.tls.as_mut().expect("TLS 配置").ca_cert = Some("invalid-certificate".into());
        let mut model: Model = serde_json::from_value(json!({
            "display_name": "probe-target", "model_name": "amazon.nova-micro-v1:0"
        }))
        .expect("模型配置");
        let request = ChatFormat::new(model.display_name.clone(), vec![]);
        assert!(!BedrockBridge::uses_reqwest_transport(
            &request,
            model.model_name.as_deref().expect("模型标识")
        ));
        assert!(
            prepare(&outbound, &model, &request, &pk, Duration::from_secs(1))
                .await
                .is_ok(),
            "SDK 轨本来不读取该 CA 配置"
        );
        model.model_name = Some("global.openai.gpt-5.6".into());
        assert!(BedrockBridge::uses_reqwest_transport(
            &request,
            model.model_name.as_deref().expect("模型标识")
        ));
        assert!(
            matches!(
                prepare(&outbound, &model, &request, &pk, Duration::from_secs(1)).await,
                Err(PrepareError::Failed(_))
            ),
            "HTTP 轨必须读取它实际使用的 CA 配置"
        );
    }

    #[tokio::test]
    async fn sdk_endpoint_resolution_keeps_partition_and_configured_userinfo() {
        let mut pk = key(Adapter::Bedrock);
        pk.api_base = None;
        pk.api_key = json!({
            "auth_type": "api_key", "region": "cn-north-1", "api_key": "ABSK-probe-test"
        })
        .to_string();
        assert_eq!(
            BedrockBridge::outbound_url(&pk).await.expect("区域端点"),
            "https://bedrock-runtime.cn-north-1.amazonaws.com.cn"
        );
        pk.api_base = Some("http://operator:configured@127.0.0.1:9/prefix".into());
        assert_eq!(
            BedrockBridge::outbound_url(&pk).await.expect("配置端点"),
            "http://operator:configured@127.0.0.1:9/prefix"
        );
    }

    #[test]
    fn token_endpoint_resolution_uses_the_same_credential_contract() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let idle = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("客户端");
        let mut azure = key(Adapter::AzureOpenai);
        azure.api_key = json!({
            "tenant_id": "probe-tenant",
            "client_id": "probe-client",
            "client_secret": "probe-secret",
            "authority_host": "http://identity:configured@127.0.0.1:8"
        })
        .to_string();
        let (_, token) = AzureOpenAiBridge::with_client(idle.clone())
            .outbound_urls(&azure, "deployment", false)
            .expect("Azure 地址");
        assert_eq!(
            token.as_deref(),
            Some("http://identity:configured@127.0.0.1:8/probe-tenant/oauth2/v2.0/token")
        );
        let mut vertex = key(Adapter::Vertex);
        vertex.api_key = json!({
            "project": "probe-project",
            "region": "global",
            "service_account_json": {
                "type": "service_account",
                "private_key": "-----BEGIN PRIVATE KEY-----\nprobe\n-----END PRIVATE KEY-----",
                "client_email": "probe@example.test",
                "token_uri": "http://identity:configured@127.0.0.1:8/token"
            }
        })
        .to_string();
        let (_, token) = VertexBridge::with_client(idle)
            .outbound_urls(&vertex)
            .expect("Vertex 地址");
        assert_eq!(
            token.as_deref(),
            Some("http://identity:configured@127.0.0.1:8/token")
        );
    }
}
