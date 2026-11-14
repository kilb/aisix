use std::error::Error;

use aisix_pricing::{
    price_event, BaselineRow, BillableUsage, Dimension, GroupRateRow, Outcome, Payer, PriceBook,
    PriceRows, PricedEvent, PricingError, Rate, ENGINE_VERSION,
};
use chrono::{DateTime, Utc};
use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const MODEL: &str = "model-fast-incident";
const INCIDENT_AT: &str = "2026-11-12T12:07:26.089Z";
const REVISION_AT: &str = "2026-11-14T00:00:00Z";

// 沿用故障卡的完整普通档和批量档，金额仍通过测试专用分组换算。
fn incomplete_card() -> Result<Value, serde_json::Error> {
    serde_json::from_str(include_str!("fixtures/fast_incident/current-card.json"))
}

// 官方 2026-11-13 价目：fast 为 $8/$40；缓存乘数为 0.05、1.25、2。
// https://platform.claude.com/docs/en/about-claude/pricing#fast-mode-pricing
fn complete_card() -> Result<Value, serde_json::Error> {
    serde_json::from_str(include_str!("fixtures/fast_incident/candidate-card.json"))
}

fn timestamp(raw: &str) -> Result<DateTime<Utc>, chrono::ParseError> {
    raw.parse()
}

fn book(with_future_revision: bool) -> Result<PriceBook, Box<dyn Error>> {
    let mut baselines = vec![
        BaselineRow {
            revision_id: 1,
            engine_version: ENGINE_VERSION,
            model_id: MODEL.into(),
            effective_from: timestamp("2026-11-10T08:14:36Z")?,
            card: complete_card()?,
        },
        BaselineRow {
            revision_id: 25,
            engine_version: ENGINE_VERSION,
            model_id: MODEL.into(),
            effective_from: timestamp("2026-11-10T09:11:23Z")?,
            card: incomplete_card()?,
        },
    ];
    if with_future_revision {
        baselines.push(BaselineRow {
            revision_id: 26,
            engine_version: ENGINE_VERSION,
            model_id: MODEL.into(),
            effective_from: timestamp(REVISION_AT)?,
            card: complete_card()?,
        });
    }
    let book = PriceBook::from_rows(PriceRows {
        baselines,
        rates: vec![GroupRateRow {
            revision_id: 101,
            engine_version: ENGINE_VERSION,
            group_id: "group-synthetic".into(),
            model_id: None,
            effective_from: timestamp("2026-11-10T00:00:00Z")?,
            cny_per_usd_e4: Some(10_000),
        }],
        cost_rates: vec![],
        discounts: vec![],
    });
    assert!(book.invalid_revisions().is_empty());
    Ok(book)
}

// 只重现已脱敏的上游计数，不复制租户、密钥、渠道或真实分组价格。
fn incident_usage(at: &str) -> Result<BillableUsage, chrono::ParseError> {
    Ok(BillableUsage {
        event_id: "event-fast-incident".into(),
        occurred_at: timestamp(at)?,
        team_id: "tenant-synthetic".into(),
        requested_model_id: MODEL.into(),
        operation: "messages".into(),
        status_code: 200,
        prompt_tokens: 4,
        completion_tokens: 5_343,
        cache_read_tokens: 796_646,
        service_tier: Some("fast".into()),
        ..BillableUsage::default()
    })
}

fn price(
    book: &PriceBook,
    usage: &BillableUsage,
    received_at: DateTime<Utc>,
) -> Result<PricedEvent, PricingError> {
    price_event(
        book,
        usage,
        &Payer {
            tenant_id: "tenant-synthetic".into(),
            group_id: Some("group-synthetic".into()),
        },
        received_at,
    )
}

#[test]
fn a_fast_incident_never_falls_back_to_an_older_or_default_tier() -> TestResult {
    // 若旧卡或普通档被静默用于 fast，这个明确错误断言必须失败。
    let usage = incident_usage(INCIDENT_AT)?;
    assert_eq!(
        price(&book(false)?, &usage, usage.occurred_at),
        Err(PricingError::MissingTierPrice {
            tier: "fast".into()
        })
    );
    Ok(())
}

#[test]
fn a_future_fast_revision_keeps_past_and_delayed_events_unpriced() -> TestResult {
    // 若价格选择错用摄入时间、或新卡提前生效，历史请求将错误地得到收费。
    let book = book(true)?;
    for at in [INCIDENT_AT, "2026-11-13T23:59:59.999999999Z"] {
        let usage = incident_usage(at)?;
        assert_eq!(
            price(&book, &usage, timestamp("2026-11-14T01:00:00Z")?),
            Err(PricingError::MissingTierPrice {
                tier: "fast".into()
            }),
            "{at}"
        );
    }
    Ok(())
}

#[test]
fn fast_uses_the_explicit_revision_at_its_exact_effective_instant() -> TestResult {
    let usage = incident_usage(REVISION_AT)?;
    let priced = price(&book(true)?, &usage, usage.occurred_at)?;
    assert_eq!(priced.outcome, Outcome::Charged);
    assert_eq!(priced.baseline.map(|b| b.revision_id), Some(26));
    assert_eq!(
        priced.service_tier.map(|tier| tier.to_string()),
        Some("fast".into())
    );
    // 测试分组固定 1 CNY/USD、无折扣：4×8 + 5343×40 + 796646×0.4 = 532410.4 µ¥。
    assert_eq!(priced.charge.0, 532_411);
    assert_eq!(priced.long_context_above, None);
    assert_eq!(
        priced
            .lines
            .iter()
            .map(|line| (line.dimension, line.quantity, line.rate))
            .collect::<Vec<_>>(),
        vec![
            (Dimension::Input, 4, Rate(8_000_000)),
            (Dimension::CacheRead, 796_646, Rate(400_000)),
            (Dimension::Output, 5_343, Rate(40_000_000)),
        ]
    );
    Ok(())
}

#[test]
fn adding_fast_keeps_default_standard_and_batch_charges_unchanged() -> TestResult {
    // 若整卡修订遗漏既有档位或把请求档位当实际档位，金额或档位断言必须失败。
    let before = book(false)?;
    let after = book(true)?;
    for (served, is_batch, normalized, amount) in [
        ("default", false, "default", 266_206),
        ("standard", false, "default", 266_206),
        ("standard", true, "batch", 133_103),
    ] {
        let mut usage = incident_usage(REVISION_AT)?;
        usage.service_tier = Some(served.into());
        usage.requested_service_tier = (!is_batch).then(|| "fast".into());
        usage.is_batch = is_batch;
        let old = price(&before, &usage, usage.occurred_at)?;
        let new = price(&after, &usage, usage.occurred_at)?;
        assert_eq!(
            (old.charge.0, new.charge.0),
            (amount, amount),
            "{served}/{is_batch}"
        );
        assert_eq!(old.lines, new.lines);
        assert_eq!(
            new.service_tier.map(|tier| tier.to_string()),
            Some(normalized.into())
        );
    }
    Ok(())
}

#[test]
fn fast_cache_write_lifetimes_and_search_use_the_reviewed_card_dimensions() -> TestResult {
    // 若缓存写入混档、1h 被按 5m 计费或遗漏搜索，手算金额及每维费率会失败。
    let mut usage = incident_usage(REVISION_AT)?;
    usage.prompt_tokens = 100;
    usage.completion_tokens = 100;
    usage.cache_read_tokens = 100;
    usage.cache_creation_tokens = 200;
    usage.cache_creation_5m_tokens = Some(100);
    usage.cache_creation_1h_tokens = Some(100);
    usage.web_search_requests = 1;
    let priced = price(&book(true)?, &usage, usage.occurred_at)?;
    // 100×8 + 100×0.4 + 100×10 + 100×16 + 100×40 + 10000 = 17440 µ¥。
    assert_eq!(priced.charge.0, 17_440);
    for (dimension, quantity, rate) in [
        (Dimension::CacheWrite, 100, Rate(10_000_000)),
        (Dimension::CacheWrite1h, 100, Rate(16_000_000)),
        (Dimension::WebSearch, 1, Rate(10_000_000_000)),
    ] {
        assert!(
            priced.lines.iter().any(|line| line.dimension == dimension
                && line.quantity == quantity
                && line.rate == rate),
            "missing {dimension:?}"
        );
    }
    Ok(())
}
