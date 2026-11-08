-- A public model is the name a tenant sends, and its id is the gateway's
-- model id for the routing document generated from it. Minted once here and
-- never changed: grants (tenants.allowed_model_ids), baselines and group
-- rates all key on it, so a new id would lose every one of them.
CREATE TABLE public_models (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  name text NOT NULL CONSTRAINT public_models_name_key UNIQUE
    CHECK (char_length(name) BETWEEN 1 AND 128),
  enabled boolean NOT NULL DEFAULT true,
  created_by uuid REFERENCES operators(id),
  created_at timestamptz NOT NULL DEFAULT now(),
  retired_at timestamptz);
-- Which public models one upstream account serves, and how. `direct_model_id`
-- is the generated direct document's id, minted with this row so a
-- regeneration replaces that document instead of creating another.
-- `provider_key_id` is checked by the writing code, under the resources lock,
-- not by a foreign key: gw_resources is keyed (kind, id) and a delete leaves
-- a tombstone there.
CREATE TABLE channel_models (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  provider_key_id uuid NOT NULL,
  public_model_id uuid NOT NULL REFERENCES public_models(id),
  upstream_model text CHECK (upstream_model IS NULL OR char_length(upstream_model) BETWEEN 1 AND 255),
  priority integer NOT NULL DEFAULT 0,
  weight integer NOT NULL DEFAULT 1 CHECK (weight BETWEEN 0 AND 65535),
  enabled boolean NOT NULL DEFAULT true,
  direct_model_id uuid NOT NULL UNIQUE,
  created_by uuid REFERENCES operators(id),
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (provider_key_id, public_model_id));
CREATE INDEX channel_models_by_public_model ON channel_models (public_model_id) WHERE enabled;
