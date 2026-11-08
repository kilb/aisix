-- Imports of a public price dataset, which an operator uploads. Each upload
-- is a batch; each priced entry of it, per date it takes effect, a proposal.
-- Nothing here is a price until an operator confirms a proposal, which writes
-- a `third_party` baseline (0025). A newer upload supersedes every pending
-- proposal of the older batches.
CREATE TABLE pricing_import_batches (
  id uuid PRIMARY KEY, seq bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
  dataset text NOT NULL, dataset_ref text NOT NULL,
  status text NOT NULL CHECK (status IN ('open','superseded')),
  proposals bigint NOT NULL, flagged bigint NOT NULL,
  created_by uuid NOT NULL REFERENCES operators(id), created_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE pricing_import_proposals (
  id uuid PRIMARY KEY, batch_id uuid NOT NULL REFERENCES pricing_import_batches(id),
  provider text NOT NULL, model text NOT NULL,
  effective_from timestamptz,                 -- NULL: in effect at upload
  card jsonb, flags jsonb NOT NULL DEFAULT '[]',
  pricing_url text, prices_checked date, suggested_model_id uuid,
  status text NOT NULL CHECK (status IN ('pending','confirmed','rejected','superseded')),
  decided_by uuid REFERENCES operators(id), decided_at timestamptz, reject_reason text,
  baseline_revision_id bigint REFERENCES pricing_baselines(revision_id),
  CHECK ((status = 'confirmed') = (baseline_revision_id IS NOT NULL)),
  CHECK ((status = 'rejected') = (reject_reason IS NOT NULL)),
  CHECK (card IS NOT NULL OR flags <> '[]'::jsonb));
CREATE INDEX pricing_import_proposals_batch ON pricing_import_proposals (batch_id, provider, model);
