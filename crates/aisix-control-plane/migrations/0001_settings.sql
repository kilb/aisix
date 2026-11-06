-- Control-plane settings: one row per key. `env_id` and `etcd_prefix` are
-- written once, by the first boot, and never updated.
CREATE TABLE cp_settings (
    key text PRIMARY KEY,
    value jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
