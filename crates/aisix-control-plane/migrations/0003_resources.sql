-- Gateway resource documents, the source the kine projection is built from.
-- Credentials in `doc` are sealed. A delete leaves a tombstone (`deleted`)
-- for the projector; `identity` is the field a kind is unique by among its
-- live rows. Every write bumps `version` and takes a new `seq`, so a row is
-- due for projection while `projected_version` differs from `version`.
CREATE SEQUENCE gw_resources_seq;
CREATE TABLE gw_resources (kind text NOT NULL, id uuid NOT NULL, doc jsonb NOT NULL, identity text,
  version bigint NOT NULL DEFAULT 1, seq bigint NOT NULL DEFAULT nextval('gw_resources_seq'),
  managed_by text, deleted boolean NOT NULL DEFAULT false,
  projected_version bigint, kine_mod_revision bigint, kine_revision bigint,
  updated_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY (kind, id));
CREATE UNIQUE INDEX gw_resources_identity ON gw_resources (kind, identity) WHERE NOT deleted AND identity IS NOT NULL;
CREATE INDEX gw_resources_dirty ON gw_resources (seq) WHERE projected_version IS DISTINCT FROM version;
