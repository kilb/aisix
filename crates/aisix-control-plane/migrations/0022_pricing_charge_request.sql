-- The request id on each charge, so that a lookup by request id
-- (`GET /dp/v1/keys/{key_id}/requests`, the call log's `?request_id=`) reads
-- the charges of one tenant only. Until now the id lived only on
-- `usage_events`, whose `team_id` is the row's claim rather than the tenant a
-- charge is filed under, so a lookup read every tenant's rows under the id,
-- and its time grew with other tenants' use of it. Intake writes the column
-- with the charge, the same value as `usage_events.request_id`; the index
-- serves the lookup newest first.
--
-- The backfill and the index are single statements, with no batching and no
-- CONCURRENTLY: the platform's first deployment starts with an empty table.
-- On a populated table both would hold usage intake for the whole rewrite,
-- and would have to be run out of band.
ALTER TABLE pricing_charges ADD COLUMN request_id text;
UPDATE pricing_charges pc SET request_id = ue.request_id
  FROM usage_events ue
  WHERE ue.event_id = pc.event_id AND ue.request_id IS NOT NULL;
CREATE INDEX pricing_charges_tenant_request
  ON pricing_charges (tenant_id, request_id, occurred_at, seq);
