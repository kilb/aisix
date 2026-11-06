-- Usage events as gateways deliver them over POST /dp/telemetry. A batch is
-- recorded once per (dp_id, batch_id), in the same transaction as its
-- events, so a re-sent batch is recognised and stores nothing twice; an event
-- is stored once per event_id, whichever batch carries it. Each event is kept
-- whole in `event`; team_id, api_key_id and request_id are copied out only to
-- be filtered on. Events that cannot be stored (no event_id, no RFC 3339
-- occurred_at, not an object) are kept as sent in usage_rejects. `seq`
-- orders rows for paging; it is allocated before commit, so it is never an
-- ingest cursor. The (column, seq) indexes serve the operators' filtered
-- pages, which run newest first by seq.
CREATE TABLE usage_batches (seq bigint GENERATED ALWAYS AS IDENTITY UNIQUE, dp_id uuid NOT NULL, batch_id uuid NOT NULL,
  received_at timestamptz NOT NULL DEFAULT now(), events bigint NOT NULL DEFAULT 0, inserted bigint NOT NULL DEFAULT 0,
  duplicates bigint NOT NULL DEFAULT 0, rejected bigint NOT NULL DEFAULT 0, deliveries bigint NOT NULL DEFAULT 1,
  PRIMARY KEY (dp_id, batch_id));
CREATE TABLE usage_events (seq bigint GENERATED ALWAYS AS IDENTITY UNIQUE, event_id text PRIMARY KEY,
  dp_id uuid NOT NULL, batch_id uuid NOT NULL, occurred_at timestamptz NOT NULL, received_at timestamptz NOT NULL DEFAULT now(),
  team_id text, api_key_id text, request_id text, event jsonb NOT NULL);
CREATE INDEX usage_events_team ON usage_events (team_id, seq);
CREATE INDEX usage_events_key ON usage_events (api_key_id, seq);
CREATE INDEX usage_events_request ON usage_events (request_id, seq);
CREATE TABLE usage_rejects (seq bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, dp_id uuid NOT NULL, batch_id uuid NOT NULL,
  reason text NOT NULL, raw jsonb NOT NULL, received_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX usage_rejects_batch ON usage_rejects (batch_id, seq);
