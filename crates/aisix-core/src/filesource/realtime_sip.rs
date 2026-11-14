//! 文件入口与托管入口使用相同的固定身份、直连账号和项目命名空间。

use std::collections::HashSet;

use crate::models::{ApiKey, Model, ProviderKey};

use super::LoadError;

pub(super) fn check(
    providers: &[(String, String, ProviderKey)],
    models: &[(String, String, Model)],
    callers: &[(String, String, ApiKey)],
) -> Vec<LoadError> {
    let mut routes = HashSet::new();
    let mut secrets = HashSet::new();
    let mut namespaces = HashSet::new();
    let mut errors = Vec::new();
    for (provider_id, scope, provider) in providers {
        let Some(binding) = &provider.realtime_sip else {
            continue;
        };
        let mut fail = |message: &str| {
            errors.push(LoadError {
                scope: scope.clone(),
                message: format!("realtime_sip: {message}"),
            })
        };
        if !routes.insert(&binding.route_id) {
            fail("route_id must be unique");
        }
        match binding.signing_key() {
            Ok(key) => {
                if !secrets.insert(key) {
                    fail("webhook_secret must not be reused by another route");
                }
            }
            Err(message) => fail(message),
        }
        match provider.realtime_sip_namespace() {
            Ok(namespace) => {
                if !namespaces.insert(namespace) {
                    fail("the upstream project's SIP calls must have one binding");
                }
            }
            Err(message) => fail(message),
        }
        let model = models.iter().find(|(id, _, _)| id == &binding.model_id);
        let caller = callers.iter().find(|(id, _, _)| id == &binding.api_key_id);
        let Some((_, _, model)) = model else {
            fail("model_id does not resolve to a model defined in this file");
            continue;
        };
        if model.provider_key_id.as_deref() != Some(provider_id.as_str())
            || model.is_routing()
            || model.is_ensemble()
            || model.is_semantic()
            || model.display_name.contains('*')
            || model
                .model_name
                .as_ref()
                .is_none_or(|name| name.trim().is_empty())
        {
            fail("model_id must identify an exact direct model on this provider key");
        }
        if caller.is_none() {
            fail("api_key_id does not resolve to a key defined in this file");
        }
        // 所有撤权形态仍能加载，不要求先移除 SIP 配置；运行期逐次检查当前权限。
    }
    errors
}
