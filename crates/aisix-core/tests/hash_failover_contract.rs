use aisix_core::models::schema::{model_root_schema, validate_model, validate_model_lenient};
use aisix_core::Routing;
use serde_json::{json, Value};

fn model(routing: Value) -> Value {
    json!({"display_name":"sessions","routing":routing})
}

#[test]
fn explicit_false_survives_lenient_read_and_serialization() {
    // 删除字段或把 false 当成缺省会使已配置的会话重新参与切换。
    let value = json!({"strategy":"consistent_hash","targets":[{"model":"a"}],
        "hash_on":[{"type":"header","name":"session-id"}],"hash_failover":false});
    validate_model_lenient(&model(value.clone())).unwrap();
    let routing: Routing = serde_json::from_value(value).unwrap();
    assert_eq!(
        serde_json::to_value(routing).unwrap().get("hash_failover"),
        Some(&json!(false))
    );
}

#[test]
fn canonical_schema_publishes_optional_true_default() {
    // 缺省值必须可被表单读出；仅检查 prose 不能发现行为被改成 false。
    for strict in [true, false] {
        let schema = model_root_schema(strict);
        assert_eq!(
            schema.pointer("/definitions/Routing/properties/hash_failover/default"),
            Some(&json!(true))
        );
        assert!(!schema["definitions"]["Routing"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("hash_failover")));
    }
}

#[test]
fn strict_writes_accept_fixed_header_hash_and_reject_other_strategies() {
    let base = json!({"strategy":"consistent_hash","targets":[{"model":"a"}],
        "hash_on":[{"type":"header","name":"session-id"}],"hash_failover":false});
    validate_model(&model(base.clone())).unwrap();
    for strategy in ["round_robin", "failover", "least_busy"] {
        let mut invalid = base.clone();
        invalid["strategy"] = json!(strategy);
        assert!(validate_model(&model(invalid)).is_err(), "{strategy}");
    }
    let mut missing_sources = base;
    missing_sources.as_object_mut().unwrap().remove("hash_on");
    assert!(validate_model(&model(missing_sources)).is_err());
}

#[test]
fn absent_or_explicit_true_preserves_hash_failover() {
    for configured in [None, Some(true), Some(false)] {
        let mut value = json!({"strategy":"consistent_hash","targets":[]});
        if let Some(value_to_set) = configured {
            value["hash_failover"] = json!(value_to_set);
        }
        let routing: Routing = serde_json::from_value(value).unwrap();
        assert_eq!(
            routing.hash_failover_or_default(),
            configured.unwrap_or(true)
        );
        assert_eq!(routing.hash_failover, configured);
    }
}
