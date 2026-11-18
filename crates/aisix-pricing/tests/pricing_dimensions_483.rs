use aisix_pricing::{
    price_event, BaselineRow, BillableUsage, GroupRateRow, Payer, PriceBook, PriceRows, RateCard,
    ENGINE_VERSION,
};
use chrono::DateTime;
use serde_json::{json, Value};

fn book(card: Value) -> PriceBook {
    PriceBook::from_rows(PriceRows {
        baselines: vec![BaselineRow {
            revision_id: 1,
            engine_version: ENGINE_VERSION,
            model_id: "m".into(),
            effective_from: DateTime::UNIX_EPOCH,
            card,
        }],
        rates: vec![GroupRateRow {
            revision_id: 2,
            engine_version: ENGINE_VERSION,
            group_id: "g".into(),
            model_id: None,
            effective_from: DateTime::UNIX_EPOCH,
            cny_per_usd_e4: Some(10_000),
        }],
        cost_rates: vec![],
        discounts: vec![],
    })
}

fn usage(extra: Value) -> BillableUsage {
    let mut row = json!({
        "event_id":"e", "occurred_at":"1970-01-01T00:00:01Z",
        "requested_model_id":"m", "team_id":"t", "operation":"chat", "status_code":200
    });
    row.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    serde_json::from_value(row).unwrap()
}

fn payer() -> Payer {
    Payer {
        tenant_id: "t".into(),
        group_id: Some("g".into()),
    }
}

fn limited_card() -> Value {
    json!({
        "default":{"base":{"input":"2.5","output":"15","cache_read":"0.25"},
            "long_context":[{"above_input_tokens":272000,
                "rates":{"input":"5","output":"22.5","cache_read":"0.5"}}]},
        "priority":{"base":{"input":"5","output":"30","cache_read":"0.5"},
            "unpriced_above_input_tokens":272000}
    })
}

#[test]
fn a_published_short_tier_prices_through_its_limit_and_refuses_long_prompts() {
    let card = limited_card();
    let decoded: RateCard = serde_json::from_value(card.clone()).unwrap();
    decoded.validate().unwrap();
    let book = book(card);
    for (input, cached, expected) in [
        (1000, 0, 5000),
        (272000, 0, 1360000),
        (272000, 1000, 1355500),
    ] {
        let u = usage(
            json!({"service_tier":"priority", "prompt_tokens":input,"cached_prompt_tokens":cached}),
        );
        let priced = price_event(&book, &u, &payer(), DateTime::UNIX_EPOCH).unwrap();
        assert_eq!(priced.charge.0, expected);
        assert_eq!(priced.service_tier.unwrap().as_str(), "priority");
    }
    let u = usage(json!({"service_tier":"priority", "prompt_tokens":272001}));
    let error = price_event(&book, &u, &payer(), DateTime::UNIX_EPOCH).unwrap_err();
    assert_eq!(error.code(), "missing_long_context_price");
    let quote = book
        .customer_quote("m", &payer(), DateTime::UNIX_EPOCH)
        .unwrap();
    let encoded = serde_json::to_value(&quote.prices_by_tier).unwrap();
    assert_eq!(encoded["priority"]["unpriced_above_input_tokens"], 272000);
    let baseline = serde_json::to_value(book.baseline_quote("m", DateTime::UNIX_EPOCH)).unwrap();
    assert_eq!(baseline["priority"]["unpriced_above_input_tokens"], 272000);
}

#[test]
fn a_short_only_marker_cannot_hide_a_bad_or_partial_card() {
    for (path, value) in [
        ("/priority/unpriced_above_input_tokens", json!(0)),
        ("/priority/unpriced_above_input_tokens", json!(271999)),
        ("/default/unpriced_above_input_tokens", json!(272000)),
        (
            "/priority/long_context",
            json!([{"above_input_tokens":272000,"rates":{"input":"10","output":"45","cache_read":"1"}}]),
        ),
    ] {
        let mut card = limited_card();
        if path == "/default/unpriced_above_input_tokens" {
            card["default"]["unpriced_above_input_tokens"] = value;
        } else if path == "/priority/long_context" {
            card["priority"]["long_context"] = value;
        } else {
            *card.pointer_mut(path).unwrap() = value;
        }
        let card: RateCard = serde_json::from_value(card).unwrap();
        assert!(card.validate().is_err(), "{path}");
    }
}

fn cache_card() -> Value {
    json!({"default":{"base":{"input":"1","output":"2","cache_read":"0.1",
        "cache_write":"1.25","cache_write_12h":"2.5"}}})
}

#[test]
fn mixed_cache_write_lifetimes_bill_each_disjoint_subset_once() {
    // 官方 usage 示例：1000 普通输入 + 4000 读 + 3000 短写 + 4000 长写。
    let book = book(cache_card());
    let u = usage(json!({"prompt_tokens":12000,"cached_prompt_tokens":4000,
        "cache_write_tokens":7000,"cache_write_30m_tokens":3000,"cache_write_12h_tokens":4000}));
    let priced = price_event(&book, &u, &payer(), DateTime::UNIX_EPOCH).unwrap();
    assert_eq!(priced.charge.0, 15150);
    let lines = serde_json::to_value(priced.lines).unwrap();
    assert!(lines
        .as_array()
        .unwrap()
        .iter()
        .any(|line| line["dimension"] == "cache_write_12h" && line["quantity"] == 4000));
}

#[test]
fn an_unreported_split_keeps_legacy_prices_but_a_reported_bad_split_is_refused() {
    let book = book(cache_card());
    let legacy = usage(json!({"prompt_tokens":1000,"cache_write_tokens":1000}));
    assert_eq!(
        price_event(&book, &legacy, &payer(), DateTime::UNIX_EPOCH)
            .unwrap()
            .charge
            .0,
        1250
    );
    for extra in [
        json!({"cache_write_30m_tokens":0,"cache_write_12h_tokens":1000}),
        json!({"cache_write_30m_tokens":1000,"cache_write_12h_tokens":0}),
    ] {
        let mut row = json!({"prompt_tokens":1000,"cache_write_tokens":1000});
        row.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(price_event(&book, &usage(row), &payer(), DateTime::UNIX_EPOCH).is_ok());
    }
    for extra in [
        json!({"cache_write_30m_tokens":1000}),
        json!({"cache_write_12h_tokens":1000}),
        json!({"cache_write_30m_tokens":400,"cache_write_12h_tokens":500}),
        json!({"cache_write_30m_tokens":500,"cache_write_12h_tokens":600}),
    ] {
        let mut row = json!({"prompt_tokens":1000,"cache_write_tokens":1000});
        row.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let error = price_event(&book, &usage(row), &payer(), DateTime::UNIX_EPOCH).unwrap_err();
        assert_eq!(error.code(), "inconsistent_usage");
    }
}

#[test]
fn a_twelve_hour_write_without_its_price_never_falls_back_to_short_writes() {
    let book = book(json!({"default":{"base":{"input":"1","output":"2","cache_write":"1.25"}}}));
    let u = usage(json!({"prompt_tokens":1000,"cache_write_tokens":1000,
        "cache_write_30m_tokens":0,"cache_write_12h_tokens":1000}));
    let error = price_event(&book, &u, &payer(), DateTime::UNIX_EPOCH).unwrap_err();
    assert_eq!(error.code(), "missing_dimension_price");
}

#[test]
fn the_builtin_published_rates_charge_short_priority_and_mixed_write_usage() {
    let catalogue = aisix_pricing::Catalogue::builtin().unwrap();
    let at = "2026-11-18T00:00:00Z".parse().unwrap();
    let entry = catalogue.get("openai", "gpt-5.4", at).unwrap();
    let book = book(serde_json::to_value(&entry.card).unwrap());
    let short =
        usage(json!({"service_tier":"priority","prompt_tokens":1000,"completion_tokens":100}));
    assert_eq!(
        price_event(&book, &short, &payer(), DateTime::UNIX_EPOCH)
            .unwrap()
            .charge
            .0,
        8000
    );
    let long = usage(json!({"service_tier":"fast","prompt_tokens":272001}));
    assert_eq!(
        price_event(&book, &long, &payer(), DateTime::UNIX_EPOCH)
            .unwrap_err()
            .code(),
        "missing_long_context_price"
    );
    let entry = catalogue.get("openai", "gpt-6-astra", at).unwrap();
    let book = self::book(serde_json::to_value(&entry.card).unwrap());
    let mixed = usage(
        json!({"prompt_tokens":12000,"cached_prompt_tokens":4000,"cache_write_tokens":7000,
        "cache_write_30m_tokens":3000,"cache_write_12h_tokens":4000,"completion_tokens":100}),
    );
    assert_eq!(
        price_event(&book, &mixed, &payer(), DateTime::UNIX_EPOCH)
            .unwrap()
            .charge
            .0,
        156500
    );
}
