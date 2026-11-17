use serde::{Deserialize, Serialize};

use crate::resource::Resource;

/// 为环境的免费令牌计数接口选择固定账号。未配置的接口沿用模型账号。
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TokenCounting {
    /// Messages 令牌计数使用的账号与模型映射。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages_count_tokens: Option<TokenCountTarget>,
    /// Responses 输入令牌计数使用的账号与模型映射。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responses_input_tokens: Option<TokenCountTarget>,
    #[serde(skip)]
    pub(crate) runtime_id: String,
}

/// 一个计数接口的固定账号。映射以源模型资源 ID 唯一标识。
#[derive(Debug, Clone, Default, Serialize, schemars::JsonSchema)]
pub struct TokenCountTarget {
    /// 固定计数账号的 Provider Key 资源 ID。
    #[serde(default)]
    #[schemars(length(min = 1))]
    pub provider_key_id: String,
    /// 源模型到固定账号模型的显式映射。未列出的模型保留源上游模型名。
    #[serde(default)]
    pub model_mappings: Vec<TokenCountModelMapping>,
    #[serde(skip)]
    model_index: std::collections::HashMap<String, usize>,
    #[serde(skip)]
    invalid_mappings: bool,
}

impl TokenCountTarget {
    /// 创建配置时一次构建索引，请求路径只借用匹配项。
    pub fn new(provider_key_id: String, model_mappings: Vec<TokenCountModelMapping>) -> Self {
        let mut model_index = std::collections::HashMap::with_capacity(model_mappings.len());
        let mut invalid_mappings = false;
        for (index, mapping) in model_mappings.iter().enumerate() {
            invalid_mappings |= mapping.model_id.is_empty()
                || mapping.source_model_name.is_empty()
                || mapping.model_name.is_empty()
                || model_index
                    .insert(mapping.model_id.clone(), index)
                    .is_some();
        }
        Self {
            provider_key_id,
            model_mappings,
            model_index,
            invalid_mappings,
        }
    }

    /// 配置解码时验证的映射完整性；重复或空字段不能被当作未配置映射。
    pub fn mappings_valid(&self) -> bool {
        !self.invalid_mappings && self.model_index.len() == self.model_mappings.len()
    }

    /// 按源模型资源 ID 读取映射，不分配内存。
    pub fn mapping_for(&self, model_id: &str) -> Option<&TokenCountModelMapping> {
        self.model_index
            .get(model_id)
            .and_then(|index| self.model_mappings.get(*index))
            .filter(|mapping| mapping.model_id == model_id)
    }
}

impl<'de> Deserialize<'de> for TokenCountTarget {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct WireTarget {
            #[serde(default)]
            provider_key_id: String,
            #[serde(default)]
            model_mappings: Vec<TokenCountModelMapping>,
        }
        let wire = WireTarget::deserialize(deserializer)?;
        Ok(Self::new(wire.provider_key_id, wire.model_mappings))
    }
}

/// 源模型的精确版本与固定账号模型之间的映射。
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TokenCountModelMapping {
    /// 源直接模型的资源 ID，不接受通配符。
    #[serde(default)]
    #[schemars(length(min = 1))]
    pub model_id: String,
    /// 保存映射时源模型的上游模型名；源模型版本变化后映射失效。
    #[serde(default)]
    #[schemars(length(min = 1))]
    pub source_model_name: String,
    /// 固定计数账号使用的上游模型名。
    #[serde(default)]
    #[schemars(length(min = 1))]
    pub model_name: String,
}

impl Resource for TokenCounting {
    fn id(&self) -> &str {
        &self.runtime_id
    }
    fn name(&self) -> &str {
        "token_counting"
    }
    fn kind() -> &'static str {
        "token_counting"
    }
}

/// 固定计数账号是否直接提供指定协议的接口。
pub fn token_count_provider_serves(key: &super::ProviderKey, surface: super::ApiSurface) -> bool {
    use super::{Adapter, ApiSurface};
    match key.adapter {
        Some(Adapter::Bedrock | Adapter::Vertex | Adapter::AzureOpenai) => return false,
        Some(Adapter::Openai | Adapter::Anthropic) | None => {}
    }
    match surface {
        ApiSurface::Messages => {
            key.provider == "anthropic"
                || key.adapter == Some(Adapter::Anthropic)
                || key
                    .apis
                    .as_ref()
                    .is_some_and(|apis| apis.messages.is_some())
        }
        ApiSurface::Responses => match &key.apis {
            Some(apis) => apis.responses.is_some(),
            None => key.provider == "openai" || key.adapter == Some(Adapter::Openai),
        },
    }
}
