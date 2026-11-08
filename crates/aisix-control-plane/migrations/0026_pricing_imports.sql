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
  attested_by uuid REFERENCES operators(id),
  baseline_revision_id bigint REFERENCES pricing_baselines(revision_id),
  CHECK ((status = 'confirmed') = (baseline_revision_id IS NOT NULL)),
  CHECK ((status = 'rejected') = (reject_reason IS NOT NULL)),
  CHECK (card IS NOT NULL OR flags <> '[]'::jsonb));
CREATE INDEX pricing_import_proposals_batch ON pricing_import_proposals (batch_id, provider, model);

-- A proposal is decided once, by the route that decides it, and what it was
-- uploaded as never changes afterwards: a confirmation's attestation names a
-- card, so a card rewritten after it would make the attestation describe
-- something that was never reviewed. `pending` moves to one of the three
-- decisions and nothing moves out of them; `card` and `flags` are the
-- upload's; `attested_by` is written once, with the confirmation.
CREATE FUNCTION pricing_import_proposal_transition() RETURNS trigger AS $$
BEGIN
  IF TG_OP = 'DELETE' THEN
    RAISE EXCEPTION 'a proposal is decided, never deleted (id %)', OLD.id;
  END IF;
  IF NEW.id <> OLD.id OR NEW.batch_id <> OLD.batch_id
     OR NEW.provider <> OLD.provider OR NEW.model <> OLD.model
     OR NEW.effective_from IS DISTINCT FROM OLD.effective_from
     OR NEW.card IS DISTINCT FROM OLD.card
     OR NEW.flags IS DISTINCT FROM OLD.flags THEN
    RAISE EXCEPTION 'a proposal''s card, flags and identity are written once (id %)', OLD.id;
  END IF;
  -- An attestation alone, on a still-pending proposal, is nobody's write.
  IF NEW.attested_by IS DISTINCT FROM OLD.attested_by AND NEW.status <> 'confirmed' THEN
    RAISE EXCEPTION 'only a confirmation attests a proposal (id %)', OLD.id;
  END IF;
  IF OLD.status <> 'pending' THEN
    RAISE EXCEPTION 'proposal % is %, which is final', OLD.id, OLD.status;
  END IF;
  IF NEW.status NOT IN ('confirmed', 'rejected', 'superseded') THEN
    RAISE EXCEPTION 'proposal % cannot move from pending to %', OLD.id, NEW.status;
  END IF;
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER pricing_import_proposals_transition
  BEFORE UPDATE OR DELETE ON pricing_import_proposals
  FOR EACH ROW EXECUTE FUNCTION pricing_import_proposal_transition();
-- A row trigger never fires on TRUNCATE, so it is refused on its own, as for
-- the ledger.
CREATE TRIGGER pricing_import_proposals_no_truncate BEFORE TRUNCATE ON pricing_import_proposals
  FOR EACH STATEMENT EXECUTE FUNCTION billing_append_only();
