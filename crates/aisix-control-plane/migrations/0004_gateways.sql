-- Secrets sealed with the master key (the CA private key, the platform id
-- key), and the client certificates issued to gateways. A certificate's
-- private key is never stored.
CREATE TABLE cp_secrets (name text PRIMARY KEY, sealed bytea NOT NULL, created_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE dp_credentials (dp_id uuid PRIMARY KEY, name text NOT NULL, serial text NOT NULL,
  fingerprint_sha256 text NOT NULL UNIQUE, not_before timestamptz NOT NULL, not_after timestamptz NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(), created_by uuid, revoked_at timestamptz);
