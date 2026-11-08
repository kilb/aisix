-- The request id on each charge, so that a lookup by request id
-- (`GET /dp/v1/keys/{key_id}/requests`, the call log's `?request_id=`) reads
-- the charges of one tenant only. Until now the id lived only on
-- `usage_events`, whose `team_id` is the row's claim rather than the tenant a
-- charge is filed under, so a lookup read every tenant's rows under the id,
-- and its time grew with other tenants' use of it. Intake writes the column
-- with the charge, the same value as `usage_events.request_id`; the index
-- serves the lookup newest first.
--
-- This fills a lookup column and changes no amount; correcting a charge is
-- still re-pricing's adjustments, never an UPDATE.
--
-- A writer that predates the column (an older instance beside a newer one in
-- a rolling upgrade, or one rolled back to) inserts charges without it, after
-- the backfill below has run. The trigger fills it from the charge's own
-- usage row, so every charge carries its request id whoever wrote it.
--
-- Every statement is idempotent. The boot runs this file in one transaction,
-- which holds ACCESS EXCLUSIVE on `pricing_charges` from the first statement
-- to the end, blocking intake and every read of charges; on the platform's
-- first deployment the table is empty and that is instant. On a populated
-- table, run it out of band first, then boot, whose own run is then left
-- with a rescan for the UPDATE:
--   1. the ALTER TABLE, the function and the trigger, each brief;
--   2. the UPDATE in batches by `seq`, adding
--      `AND pc.seq > <last> AND pc.seq <= <last> + 10000` and committing each;
--   3. CREATE INDEX CONCURRENTLY IF NOT EXISTS with the definition below;
--   4. SELECT indisvalid FROM pg_index
--        WHERE indexrelid = 'pricing_charges_tenant_request'::regclass;
--      when it is false, DROP INDEX CONCURRENTLY and repeat step 3.
ALTER TABLE pricing_charges ADD COLUMN IF NOT EXISTS request_id text;
CREATE OR REPLACE FUNCTION pricing_charges_request_id() RETURNS trigger LANGUAGE plpgsql AS
  $$ BEGIN
       SELECT ue.request_id INTO NEW.request_id
         FROM usage_events ue WHERE ue.event_id = NEW.event_id;
       RETURN NEW;
     END $$;
CREATE OR REPLACE TRIGGER pricing_charges_request_id BEFORE INSERT ON pricing_charges
  FOR EACH ROW WHEN (NEW.request_id IS NULL) EXECUTE FUNCTION pricing_charges_request_id();
UPDATE pricing_charges pc SET request_id = ue.request_id
  FROM usage_events ue
  WHERE ue.event_id = pc.event_id AND ue.request_id IS NOT NULL AND pc.request_id IS NULL;
CREATE INDEX IF NOT EXISTS pricing_charges_tenant_request
  ON pricing_charges (tenant_id, request_id, occurred_at, seq);
