# aisix-pricing

The control plane's pricing engine: `price_event(&PriceBook, &BillableUsage,
received_at)` turns one stored usage row into one charge. Its persistence, the
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

- Rates are integer micro-USD per million units (`Rate`), money is integer
  micro-USD (`MicroUsd`), and each row is rounded exactly once, upward, with
  the tenant discount inside that one rounding. Prices cross the API as
  decimal strings and are parsed to integers (`Rate::parse`); no `f64` or
  `f32` appears in `src/`.
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
  the engine's charge minus the gateway's is in `{0, 1}` µ$, and equal on a
  whole-µ$ row. `tests/golden.rs` prices rows a real gateway emitted
  (`golden/wire/*.json`) and checks each charge against `EXPECTED`, against
  `golden/priced/*.json`, and against the gateway's own `cost_usd` for the
  families in `PRICED_ALIKE`.
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
- The same change bumps `ENGINE_VERSION`. Every revision and charge is
  stamped with the version that wrote it, and an engine refuses a revision
  stamped newer than itself (`invalid_price`, isolated to that revision's
  model or tenant). That refusal, not the card's parse, is what stops an
  older control plane in a rolling deploy from pricing a row under rules it
  does not have: a card written for the new rules can still parse there
  while the row's new counter is ignored. The stamp on each charge also tells
  re-pricing which rules produced it.
- A quantity on a dimension the selected rates do not price refuses the row
  (`missing_dimension_price`); it is never billed at zero.

## A price never starts in the past

- Every price write refuses an `effective_from` more than
  `BACKDATE_TOLERANCE` (5 s) before the database's own `now()`
  (`crates/aisix-control-plane/src/pricing/store.rs`), for sell prices, cost
  prices and discounts alike, imports included. With no backdating, the book
  at ingest equals the book at the row's `occurred_at`, so a row that arrives
  late is priced exactly as one that arrived on time.
- Correcting past charges is re-pricing: it appends adjustments and never
  edits a charge or a revision. Do not add an `UPDATE` or `DELETE` on a
  revision table or on `pricing_charges`, and do not loosen the tolerance to
  let an operator "fix" yesterday.
