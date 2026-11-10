# aisix-pricing

The control plane's pricing engine: `price_event(&PriceBook, &BillableUsage,
&Payer, received_at)` turns one stored usage row into one charge. Its persistence, the
ingest hook and the operator API live in `crates/aisix-control-plane/src/pricing/`.
A charge is written once and never updated, so a wrong number here is not
fixed by the next release: it stays on every row priced until re-pricing
corrects it. The rules below are the ones a change can break without any test
failing unless it was written for them.

## The engine is pure

- **No I/O, no clock, no randomness and no global mutable state on the
  pricing path.** The same `PriceBook`, `BillableUsage` and `received_at`
  always give the same `PricedEvent`. The instant a row is priced at is
  derived from its own `occurred_at` and the `received_at` the caller passes,
  never from `Utc::now()`.
- The crate has no `sqlx`, no async runtime and no network dependency, and
  keeps it that way. A lookup that needs the database belongs to the control
  plane, which loads `PriceRows` and hands the engine a finished `PriceBook`.

## No floating point on the charge path

- Three integer quantities make a charge, and each row is rounded exactly
  once, upward, with the group rate and the tenant discount inside that one
  rounding:

  `charge_micro_cny = ⌈ Σ_d q_d × baseline_rate_d × cny_per_usd_e4 × (10000 − discount_bps) / 10¹⁴ ⌉`

  - a **baseline** is a model's official USD list price, in micro-USD per
    million units (`Rate`, inside a `RateCard`);
  - a **group rate** is `cny_per_usd_e4`, ten-thousandths of a CNY per USD,
    from 0 to 100 CNY (`CnyPerUsd`);
  - a **charge** is integer micro-CNY (`MicroCny`).

  An upstream's **cost** is the same lines at the serving account's cost
  rate (a `CnyPerUsd`, the unit and range of a group rate), through the same
  function with no discount, so it is rounded once the same way:

  `cost_micro_cny = ⌈ Σ_d q_d × baseline_rate_d × cost_cny_per_usd_e4 / 10¹⁰ ⌉`

  A row naming no account, an account with no cost rate in force, an
  unreadable cost-rate revision or an amount past `i64` leaves the cost
  unknown (`None`), never 0, and never refuses the charge. Prices cross the
  API as decimal strings and are parsed to integers (`Rate::parse`,
  `CnyPerUsd::parse`); no `f64` or `f32` appears in `src/`.
- Rounding first and multiplying afterwards bills a different number. The
  discriminating case is pinned in `the_rate_and_the_discount_are_inside_the_one_rounding`:
  one token at USD 0.15/M × 7.2 is 1.08 µ¥, which is 2 µ¥ with no discount and
  1 µ¥ at 1000 bps. Rounding the rate first gives ⌈1.08⌉ = 2 → ⌈1.8⌉ = 2, and
  rounding the USD amount first gives ⌈0.15⌉ = 1 µ$ → ⌈7.2⌉ = 8; only the
  implemented order, one rounding after both factors, gives 1.
- A row's total stays exact in `u128` because every quantity is bounded by
  `MAX_QUANTITY` (three `u32` counters) and every rate by `Rate::MAX`,
  `CnyPerUsd::MAX` and 10⁴ bps; `rates.rs` asserts that bound at compile time
  (ruling R1). An amount past `i64`, or a line product past `u128`, is
  `PricingError::Overflow`, never a wrapped or negative amount. A discount
  above 10000 bps is its own error (`DiscountAboveWhole`), because it is a
  writer's bug rather than an arithmetic limit; it is refused, never clamped.
- The gateway's `ModelCost` is `f64` and is used here only by the tests that
  compare against it (`tests/equivalence.rs`, `tests/golden.rs`), never to
  produce a charge. `cost_usd` on the row is never read for pricing.

## Usage arithmetic comes only from `aisix_core`, never a copy

- How a row's counters are read (the subset OpenAI-shape cache counters
  against the additive Anthropic-shape ones, the one-hour write split,
  reasoning beside the completion) is `aisix_core::input_tokens_for_pricing`,
  `aisix_core::one_hour_cache_writes` and
  `aisix_core::reasoning_beside_completion`. The gateway's own spend calls the
  same functions. Call them; do not reimplement, inline or "simplify" any of
  them here, since a second copy is how the engine and the gateway would come
  to bill one row two ways.
- A change to how a row is read is made in `aisix_core`, for both sides at
  once, and `tests/equivalence.rs` and `tests/golden.rs` must stay green
  afterwards. The equivalence property test alone cannot see a drift inside
  a function both sides call: what catches one is a hand-computed figure,
  the equivalence test's fixed rows and each golden family's `EXPECTED`
  charge. A new way a row can be read gets a golden family of its own.
- `src/` is exempt from the gateway's `no_one_reads_a_models_price_off_the_field`
  census (`crates/aisix-proxy/src/routing.rs`) only while it never names
  `Model` or `ModelCost`. A line that does is reported as a read.

## `tests/equivalence.rs` and `tests/golden.rs` are never relaxed to pass

- `tests/equivalence.rs` is a property test pinning the engine to the
  gateway's spend computation wherever `ModelCost` can express the price:
  the engine's charge minus the gateway's is in `{0, 1}` µ¥, and equal on a
  whole-µ¥ row. Both it and `tests/golden.rs` price at a group rate of
  exactly 1.0000 CNY per USD, so at rate 1 a charge in µ¥ is the baseline's
  in µ$ and the pinned amounts are the gateway's own. `tests/golden.rs`
  prices rows a real gateway emitted (`golden/wire/*.json`) and checks each
  charge against `EXPECTED`, against `golden/priced/*.json`, and against the
  gateway's own `cost_usd` for the families in `PRICED_ALIKE`.
- When either fails, the engine or the shared reading drifted. Fix that.
  Widening the tolerance, dropping a case, shrinking `PRICED_ALIKE` or
  re-recording `EXPECTED` or `golden/priced/` to match new output is not a
  fix. The wire files are recaptured only when the gateway's own output
  changed on purpose (`UPDATE_PRICING_GOLDEN=1`, both e2e modes, identical
  results), and then `EXPECTED` and `golden/priced/` must not move.

## A new dimension arrives with its usage-event field and an `ENGINE_VERSION` bump

- A `Dimension` is billable only once the gateway's usage event carries the
  quantity it meters (`aisix-obs` `UsageEvent`, read here through
  `BillableUsage`). Add the event field, its `BillableUsage` field and the
  dimension in the same change; a rate for a quantity nobody reports bills
  nothing, silently.
- The same change bumps `ENGINE_VERSION`. It is 2: version 2 prices a row at
  its model's baseline times its group's rate, with the tenant discount
  still applied, and version 1 priced it at a platform sell price. Every
  revision and charge is stamped with the version that wrote it, and an
  engine refuses a revision stamped newer than itself (`invalid_price`,
  isolated to that revision's model, group or tenant). That refusal, not the
  card's parse, is what stops an older control plane in a rolling deploy
  from pricing a row under rules it does not have: a card written for the
  new rules can still parse there while the row's new counter is ignored.
  The stamp on each charge also tells re-pricing which rules produced it.
- A quantity on a dimension the selected rates do not price refuses the row
  (`missing_dimension_price`); it is never billed at zero.

## A price never starts in the past

- Every price write refuses an `effective_from` more than
  `BACKDATE_TOLERANCE` (5 s) before the database's own `now()`
  (`crates/aisix-control-plane/src/pricing/store.rs`), for baselines, group
  rates, cost rates and discounts alike, imports included. With no
  backdating, the book at ingest equals the book at the row's `occurred_at`,
  so a row that arrives late is priced exactly as one that arrived on time.
- Correcting past charges is re-pricing: it appends adjustments and never
  edits a charge or a revision. Do not add an `UPDATE` or `DELETE` on a
  revision table or on `pricing_charges`, and do not loosen the tolerance to
  let an operator "fix" yesterday.

## What the equivalence test does not pin

- `tests/equivalence.rs` re-implements the proxy's input-token helper, so
  drift in the gateway's helper is not caught by that property test; it is
  caught by the proxy unit test `the_spend_reads_the_split_the_usage_event_reports`
  and by the e2e.
