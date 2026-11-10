-- The library entry an operator chose for a model the price library (0033)
-- matches ambiguously: one row per model. A sync that lists such a model
-- under one of its candidates writes the row in its own transaction, and
-- from then on the model matches that entry before anything its name finds.
-- A row whose entry has left the library is ignored, not deleted; an
-- operator deletes a row to choose again.
--
-- `provider` and `model` name the entry as the dataset does, never by
-- position, so a newer copy of the library finds the same entry or none.
-- `model_id` names a gateway model, as a baseline's does (0025).
CREATE TABLE pricing_library_bindings (
  model_id uuid PRIMARY KEY,
  provider text NOT NULL,
  model text NOT NULL,
  chosen_by uuid NOT NULL REFERENCES operators(id),
  chosen_at timestamptz NOT NULL DEFAULT now());
