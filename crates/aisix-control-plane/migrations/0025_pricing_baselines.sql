-- Baselines: each model's official list price in USD, as a full rate card.
-- A tenant pays it times its group's rate. Append-only; the source says
-- where the card came from: the built-in catalogue, a third-party dataset an
-- operator attested, or a manual entry with its note.
CREATE TABLE pricing_baselines (
  revision_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  engine_version bigint NOT NULL,
  model_id uuid NOT NULL,
  effective_from timestamptz NOT NULL,
  card jsonb NOT NULL,
  source_kind text NOT NULL CHECK (source_kind IN ('catalogue','third_party','manual')),
  source jsonb NOT NULL,
  created_by uuid REFERENCES operators(id),
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX pricing_baselines_lookup ON pricing_baselines (model_id, effective_from, revision_id);
CREATE TRIGGER pricing_baselines_append_only BEFORE UPDATE OR DELETE ON pricing_baselines
  FOR EACH ROW EXECUTE FUNCTION billing_append_only();
-- A row trigger never fires on TRUNCATE, so it is refused on its own, as for the ledger.
CREATE TRIGGER pricing_baselines_no_truncate BEFORE TRUNCATE ON pricing_baselines
  FOR EACH STATEMENT EXECUTE FUNCTION billing_append_only();
