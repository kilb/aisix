-- Provider groups as data. `name` is the routing name provider keys and key
-- documents carry: immutable, never shown to tenants. `display_name` and
-- `description` are what tenants see. A group is never deleted; a retired
-- one keeps its row, and its name, for history.
CREATE TABLE provider_groups (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  name text NOT NULL CONSTRAINT provider_groups_name_key UNIQUE,
  display_name text NOT NULL CHECK (char_length(display_name) BETWEEN 1 AND 64),
  description text NOT NULL DEFAULT '' CHECK (char_length(description) <= 500),
  is_default boolean NOT NULL DEFAULT false,
  created_by uuid REFERENCES operators(id),
  created_at timestamptz NOT NULL DEFAULT now(),
  retired_at timestamptz);
-- At most one group is the designated default: the one an upgrade gives
-- every internal key still without a group.
CREATE UNIQUE INDEX provider_groups_one_default ON provider_groups ((true)) WHERE is_default;
-- One group per name in use. Backfilled groups get no rate: their rows are
-- `missing_rate` until an operator sets one. The COALESCE is the first-boot
-- default (`settings::TenantDefaults::first_boot`), which a fresh database's
-- boot writes only after migrating.
INSERT INTO provider_groups (name, display_name)
  SELECT DISTINCT name, left(name, 64) FROM (
    SELECT jsonb_array_elements_text(doc->'groups') AS name FROM gw_resources
      WHERE kind = 'provider_keys' AND NOT deleted AND jsonb_typeof(doc->'groups') = 'array'
    UNION SELECT unnest(allowed_provider_groups) FROM tenants
    UNION SELECT unnest(allowed_provider_groups) FROM tenant_keys
      WHERE revoked_at IS NULL AND allowed_provider_groups IS NOT NULL
    UNION SELECT jsonb_array_elements_text(COALESCE(
      (SELECT value->'default_provider_groups' FROM cp_settings WHERE key = 'tenant_defaults'),
      '["shared"]'))
  ) names WHERE btrim(name) <> '';
-- The designated default group. A backfilled `default` becomes it; otherwise
-- one is inserted. It gets no rate, like every backfilled group.
UPDATE provider_groups SET is_default = true WHERE name = 'default' AND retired_at IS NULL;
INSERT INTO provider_groups (name, display_name, is_default)
  SELECT 'default', '默认', true
  WHERE NOT EXISTS (SELECT 1 FROM provider_groups WHERE is_default);
-- Rates: CNY per USD in ten-thousandths, per (group, model), with the
-- group's model-less row as its default. Append-only, like every price.
CREATE TABLE pricing_group_rates (
  revision_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  engine_version bigint NOT NULL,
  group_id uuid NOT NULL REFERENCES provider_groups(id),
  model_id uuid,
  effective_from timestamptz NOT NULL,
  cny_per_usd_e4 bigint CHECK (cny_per_usd_e4 BETWEEN 0 AND 1000000),
  note text NOT NULL DEFAULT '',
  created_by uuid REFERENCES operators(id),
  created_at timestamptz NOT NULL DEFAULT now(),
  CHECK (model_id IS NOT NULL OR cny_per_usd_e4 IS NOT NULL));
CREATE INDEX pricing_group_rates_lookup ON pricing_group_rates (group_id, model_id, effective_from, revision_id);
CREATE TRIGGER pricing_group_rates_append_only BEFORE UPDATE OR DELETE ON pricing_group_rates
  FOR EACH ROW EXECUTE FUNCTION billing_append_only();
CREATE TRIGGER pricing_group_rates_no_truncate BEFORE TRUNCATE ON pricing_group_rates
  FOR EACH STATEMENT EXECUTE FUNCTION billing_append_only();
