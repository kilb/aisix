-- Pricing: the price revisions operators write, and the one charge each
-- stored usage row gets, in the transaction that stores it.
--
-- The three revision tables are append-only: a price changes only by a new
-- revision starting now or later, and nothing updates or deletes a row.
-- `engine_version` is the writing engine's ENGINE_VERSION; a row from a newer
-- engine is isolated, not read. `model_id` has no foreign key, so a price
-- outlives a deleted model.
--
-- `pricing_charges` holds one row per usage_events row, written once and
-- never updated. `tenant_id`, `api_key_id` and `user_id` are copied from the
-- usage row as text with no foreign key, since a row may name an empty or
-- unknown tenant and is still stored. `seq` is for paging only, never an
-- ingest or ledger cursor. A row that cannot be priced is `unpriced`, with
-- its reason in `error_code` and no charge.
--
-- `pricing_version` in cp_settings is bumped by every price write, in the
-- same transaction, so a cached price book knows when to reload.
CREATE TABLE pricing_sell_revisions (
  revision_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  engine_version bigint NOT NULL,
  model_id uuid NOT NULL,
  tenant_id uuid REFERENCES tenants(id),
  effective_from timestamptz NOT NULL,
  card jsonb,
  source text NOT NULL,
  created_by uuid REFERENCES operators(id),
  created_at timestamptz NOT NULL DEFAULT now(),
  CHECK (card IS NOT NULL OR tenant_id IS NOT NULL));
CREATE INDEX pricing_sell_lookup ON pricing_sell_revisions (model_id, tenant_id, effective_from, revision_id);
CREATE TABLE pricing_cost_revisions (
  revision_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, engine_version bigint NOT NULL,
  upstream_provider text NOT NULL, upstream_model text NOT NULL, provider_key_id uuid,
  effective_from timestamptz NOT NULL, card jsonb NOT NULL, source text NOT NULL,
  created_by uuid REFERENCES operators(id), created_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE pricing_discount_revisions (
  revision_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, engine_version bigint NOT NULL,
  tenant_id uuid NOT NULL REFERENCES tenants(id), effective_from timestamptz NOT NULL,
  discount_bps bigint NOT NULL CHECK (discount_bps BETWEEN 0 AND 10000),
  created_by uuid REFERENCES operators(id), created_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE pricing_charges (
  seq bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
  event_id text PRIMARY KEY REFERENCES usage_events(event_id),
  tenant_id text NOT NULL, api_key_id text NOT NULL, user_id text NOT NULL,
  requested_model_id text NOT NULL, occurred_at timestamptz NOT NULL, price_instant timestamptz,
  status text NOT NULL CHECK (status IN ('charged','exempt','nothing_to_bill','unpriced')),
  charge_micro_usd bigint CHECK (charge_micro_usd >= 0),
  cost_micro_usd bigint CHECK (cost_micro_usd >= 0), cost_source jsonb,
  sell_revision_id bigint REFERENCES pricing_sell_revisions(revision_id),
  tenant_override boolean NOT NULL DEFAULT false,
  discount_revision_id bigint REFERENCES pricing_discount_revisions(revision_id),
  discount_bps bigint NOT NULL DEFAULT 0,
  service_tier text, requested_service_tier text, long_context_above bigint,
  estimated boolean NOT NULL DEFAULT false, exemption text, error_code text,
  lines jsonb NOT NULL DEFAULT '[]', warnings jsonb NOT NULL DEFAULT '[]',
  engine_version bigint NOT NULL, priced_at timestamptz NOT NULL DEFAULT now(),
  CHECK ((status = 'unpriced') = (charge_micro_usd IS NULL)));
CREATE INDEX pricing_charges_tenant ON pricing_charges (tenant_id, occurred_at);
CREATE INDEX pricing_charges_key ON pricing_charges (api_key_id, occurred_at);
CREATE INDEX pricing_charges_unpriced ON pricing_charges (seq) WHERE status = 'unpriced';
INSERT INTO cp_settings (key, value) VALUES ('pricing_version', '0');
