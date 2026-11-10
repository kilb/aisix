-- Cost rates: what an upstream account (a provider key) costs the platform,
-- in CNY per USD of a model's baseline, as ten-thousandths. The shape of
-- `pricing_group_rates` (0023), keyed on the account instead of a group: the
-- account's model-less row is its default, a model row overrides that one
-- model, and a model row with no rate ends its override. Append-only, like
-- every price.
--
-- `provider_key_id` names a `gw_resources` document, which has no foreign key
-- to point at: the write checks that the account is live, and a revision for
-- an account deleted later simply never applies.
CREATE TABLE pricing_cost_rates (
  revision_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  engine_version bigint NOT NULL,
  provider_key_id uuid NOT NULL,
  model_id uuid,
  effective_from timestamptz NOT NULL,
  cny_per_usd_e4 bigint CHECK (cny_per_usd_e4 BETWEEN 0 AND 1000000),
  note text NOT NULL DEFAULT '',
  created_by uuid REFERENCES operators(id),
  created_at timestamptz NOT NULL DEFAULT now(),
  CHECK (model_id IS NOT NULL OR cny_per_usd_e4 IS NOT NULL));
CREATE INDEX pricing_cost_rates_lookup
  ON pricing_cost_rates (provider_key_id, model_id, effective_from, revision_id);
CREATE TRIGGER pricing_cost_rates_append_only BEFORE UPDATE OR DELETE ON pricing_cost_rates
  FOR EACH ROW EXECUTE FUNCTION billing_append_only();
CREATE TRIGGER pricing_cost_rates_no_truncate BEFORE TRUNCATE ON pricing_cost_rates
  FOR EACH STATEMENT EXECUTE FUNCTION billing_append_only();

-- A charge's cost in micro-CNY: its own lines at the serving account's cost
-- rate in force at its price instant. NULL is an unknown cost (no account
-- named, no rate in force), never 0. `cost_micro_usd` stays for the rows
-- priced before this column, and is no longer written.
ALTER TABLE pricing_charges_cny ADD COLUMN cost_micro_cny bigint CHECK (cost_micro_cny >= 0);
