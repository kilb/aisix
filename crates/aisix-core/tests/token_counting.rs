use aisix_core::models::schema::resource_root_schema;
use serde_json::{json, Value};

fn validate(doc: &Value, strict: bool) -> bool {
    jsonschema::options()
        .build(&resource_root_schema("token_counting", strict))
        .unwrap()
        .is_valid(doc)
}

fn target() -> Value {
    json!({"provider_key_id":"pk", "model_mappings":[{
        "model_id":"m", "source_model_name":"claude-v1", "model_name":"claude-v1"
    }]})
}

#[test]
fn token_counting_optional_blocks_and_required_fields() {
    assert!(validate(&json!({}), true));
    for block in ["messages_count_tokens", "responses_input_tokens"] {
        let valid = json!({block:target()});
        assert!(validate(&valid, true));
        for field in ["provider_key_id"] {
            let mut missing = valid.clone();
            missing[block].as_object_mut().unwrap().remove(field);
            assert!(!validate(&missing, true), "missing {field}");
            assert!(!validate(&missing, false), "missing {field} on read");
            missing[block][field] = json!("");
            assert!(!validate(&missing, true), "empty {field}");
        }
        for field in ["model_id", "source_model_name", "model_name"] {
            for replacement in [None, Some(json!(""))] {
                let mut invalid = valid.clone();
                let mapping = invalid[block]["model_mappings"][0].as_object_mut().unwrap();
                mapping.remove(field);
                if let Some(value) = replacement {
                    mapping.insert(field.into(), value);
                }
                assert!(!validate(&invalid, true), "invalid {field}");
                assert!(!validate(&invalid, false), "invalid {field} on read");
            }
        }
    }
}

#[test]
fn token_counting_unknown_fields_are_read_tolerant_at_every_depth() {
    for path in ["root", "target", "mapping"] {
        let mut doc = json!({"messages_count_tokens": target()});
        let object = match path {
            "root" => doc.as_object_mut().unwrap(),
            "target" => doc["messages_count_tokens"].as_object_mut().unwrap(),
            _ => doc["messages_count_tokens"]["model_mappings"][0]
                .as_object_mut()
                .unwrap(),
        };
        object.insert("future_field".into(), json!(true));
        assert!(!validate(&doc, true));
        assert!(validate(&doc, false));
    }
}

#[test]
fn token_counting_duplicate_source_ids_are_rejected_in_each_block() {
    use aisix_core::models::{validate_token_counting, validate_token_counting_lenient};
    let mut block = target();
    let mapping = block["model_mappings"][0].clone();
    block["model_mappings"]
        .as_array_mut()
        .unwrap()
        .push(mapping);
    let doc = json!({"messages_count_tokens":block});
    assert!(validate_token_counting(&doc).is_err());
    assert!(validate_token_counting_lenient(&doc).is_err());
}

#[test]
fn token_counting_provider_protocol_contract() {
    use aisix_core::models::token_counting::token_count_provider_serves;
    use aisix_core::models::{ApiSurface, ProviderKey};
    for (extra, messages, responses) in [
        (json!({"provider":"anthropic"}), true, false),
        (json!({"provider":"openai"}), false, true),
        (
            json!({"provider":"custom", "adapter":"anthropic"}),
            true,
            false,
        ),
        (
            json!({"provider":"custom", "adapter":"openai"}),
            false,
            false,
        ),
        (json!({"provider":"openai", "apis":{}}), false, false),
        (
            json!({"provider":"custom", "apis":{"messages":{}, "responses":{}}}),
            true,
            true,
        ),
        (
            json!({"provider":"openai", "adapter":"azure-openai", "apis":{"messages":{}, "responses":{}}}),
            false,
            false,
        ),
        (
            json!({"provider":"anthropic", "adapter":"bedrock"}),
            false,
            false,
        ),
        (
            json!({"provider":"anthropic", "adapter":"vertex"}),
            false,
            false,
        ),
    ] {
        let mut value = json!({"display_name":"counter", "api_key":"local-only"});
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let key: ProviderKey = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            token_count_provider_serves(&key, ApiSurface::Messages),
            messages,
            "{value}"
        );
        assert_eq!(
            token_count_provider_serves(&key, ApiSurface::Responses),
            responses,
            "{value}"
        );
    }
}

#[test]
fn token_counting_shape_and_defaults_are_canonical() {
    use aisix_core::models::TokenCounting;
    use aisix_core::Resource;
    let parsed: TokenCounting = serde_json::from_value(json!({"future_root":true})).unwrap();
    assert_eq!(parsed.name(), "token_counting");
    assert_eq!(TokenCounting::kind(), "token_counting");
    assert!(parsed.messages_count_tokens.is_none());
    assert!(parsed.responses_input_tokens.is_none());
    for invalid in [
        json!({"messages_count_tokens": []}),
        json!({"messages_count_tokens": {"provider_key_id": 1}}),
        json!({"messages_count_tokens": {"provider_key_id": "pk", "model_mappings": {}}}),
        json!({"messages_count_tokens": {"provider_key_id": "pk", "model_mappings": [null]}}),
        json!({"messages_count_tokens": {"provider_key_id": "pk", "model_mappings": [{"model_id":"m","source_model_name":"v","model_name":null}]}}),
    ] {
        assert!(!validate(&invalid, true), "{invalid}");
        assert!(!validate(&invalid, false), "{invalid}");
    }
}

#[test]
fn token_counting_file_rejects_counter_with_incompatible_protocol() {
    let input = r#"{
        "_format_version":"1",
        "provider_keys":[{"display_name":"counter", "api_key":"local-only", "provider":"openai"}],
        "token_counting":[{"messages_count_tokens":{"provider_key":"counter"}}]
    }"#;
    let errors =
        aisix_core::filesource::load_from_str(input, "test.json", 1, &|_| None).unwrap_err();
    assert!(
        errors.to_string().contains("messages_count_tokens"),
        "{errors}"
    );
}

#[test]
fn token_counting_mapping_index_is_ready_after_decode_and_does_not_serialize() {
    use aisix_core::models::TokenCountTarget;
    let decoded: TokenCountTarget = serde_json::from_value(target()).unwrap();
    let mapping = decoded.mapping_for("m").unwrap();
    assert_eq!(mapping.source_model_name, "claude-v1");
    assert!(std::ptr::eq(mapping, &decoded.model_mappings[0]));
    assert!(decoded.mapping_for("missing").is_none());
    assert!(TokenCountTarget::default().mapping_for("missing").is_none());
    assert_eq!(serde_json::to_value(&decoded).unwrap(), target());
    assert_eq!(
        decoded.clone().mapping_for("m").unwrap().model_name,
        "claude-v1"
    );
}

#[test]
fn token_counting_mapping_index_marks_invalid_documents_without_request_scans() {
    use aisix_core::models::TokenCountTarget;
    assert!(TokenCountTarget::default().mappings_valid());
    assert!(serde_json::from_value::<TokenCountTarget>(target())
        .unwrap()
        .mappings_valid());
    for field in ["model_id", "source_model_name", "model_name"] {
        let mut invalid = target();
        invalid["model_mappings"][0][field] = json!("");
        assert!(!serde_json::from_value::<TokenCountTarget>(invalid)
            .unwrap()
            .mappings_valid());
    }
    let mut duplicate = target();
    let mapping = duplicate["model_mappings"][0].clone();
    duplicate["model_mappings"]
        .as_array_mut()
        .unwrap()
        .push(mapping);
    assert!(!serde_json::from_value::<TokenCountTarget>(duplicate)
        .unwrap()
        .mappings_valid());
}

#[test]
fn token_counting_required_strings_publish_no_invalid_default() {
    let schema = resource_root_schema("token_counting", true);
    for pointer in [
        "/definitions/TokenCountTarget/properties/provider_key_id",
        "/definitions/TokenCountModelMapping/properties/model_id",
        "/definitions/TokenCountModelMapping/properties/source_model_name",
        "/definitions/TokenCountModelMapping/properties/model_name",
    ] {
        assert!(
            schema.pointer(pointer).unwrap().get("default").is_none(),
            "{pointer}"
        );
    }
}

#[test]
fn token_counting_file_rejects_valid_embedding_source_models() {
    let model = json!({"display_name":"source", "provider":"openai", "model_name":"embedding-v1", "provider_key_id":"pk", "embedding":{"dimensions":1536}});
    aisix_core::models::validate_model(&model).unwrap();
    let mut file_model = model;
    file_model
        .as_object_mut()
        .unwrap()
        .remove("provider_key_id");
    file_model["provider_key"] = json!("counter");
    let input = json!({
        "_format_version":"1",
        "provider_keys":[{"display_name":"counter", "api_key":"local-only", "provider":"openai"}],
        "models":[file_model],
        "token_counting":[{"responses_input_tokens":{"provider_key":"counter", "model_mappings":[{
            "model":"source", "source_model_name":"embedding-v1", "model_name":"counter-v1"
        }]}}]
    })
    .to_string();
    assert!(aisix_core::filesource::load_from_str(&input, "embedding.json", 1, &|_| None).is_err());
}
