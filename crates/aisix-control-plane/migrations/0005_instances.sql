-- Gateway instances, one row per (credential, process): replicas started
-- from one bundle share a dp_id and report their own instance_id. Every
-- heartbeat replaces the row's reported state, the lists included. A
-- credential keeps at most a fixed number of rows; `instance_cap_hit_at` is
-- when it last had to refuse a new instance because all of them were
-- online, so the fleet seen in `dp_instances` may then be incomplete.
CREATE TABLE dp_instances (dp_id uuid NOT NULL REFERENCES dp_credentials(dp_id), instance_id text NOT NULL,
  hostname text NOT NULL DEFAULT '', version text NOT NULL DEFAULT '', uptime_seconds bigint NOT NULL DEFAULT 0,
  applied_revision bigint NOT NULL DEFAULT 0, config_hash text, supported_guardrail_kinds jsonb NOT NULL DEFAULT '[]',
  exporter_health jsonb NOT NULL DEFAULT '[]', rejected_resources jsonb NOT NULL DEFAULT '[]',
  partially_compatible_resources jsonb NOT NULL DEFAULT '[]',
  first_seen_at timestamptz NOT NULL DEFAULT now(), last_seen_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (dp_id, instance_id));
ALTER TABLE dp_credentials ADD COLUMN instance_cap_hit_at timestamptz;
