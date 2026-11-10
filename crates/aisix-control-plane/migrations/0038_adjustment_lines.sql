-- What a re-pricing run priced each charge it corrected at: the billed
-- quantities and their baseline rates (`lines`, as `pricing_charges_cny`
-- stores them), the group rate, the tenant discount and the served tier that
-- turned them into the corrected amount.
--
-- A charge is written once, so a row stored unpriced keeps its empty `lines`
-- after a run charges it. The call log, the usage summary and the
-- request-cost lookup read these columns of the charge's latest adjustment
-- instead, so a corrected call shows the quantities it was billed for.
--
-- Additive and nullable: an adjustment applied before this migration carries
-- none, and its charge keeps showing its own `lines`. The table stays
-- append-only, so nothing backfills those rows.
ALTER TABLE pricing_adjustments_cny
  ADD COLUMN lines jsonb,
  ADD COLUMN cny_per_usd_e4 bigint,
  ADD COLUMN discount_bps bigint,
  ADD COLUMN service_tier text;
