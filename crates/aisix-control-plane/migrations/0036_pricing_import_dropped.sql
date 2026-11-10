-- The price fields a proposal's card leaves off, by the dataset's own name
-- (0026). A quantity no card dimension meters is dropped and no longer blocks
-- a confirmation, so the operator attesting the card has to be able to see
-- what it does not bill; the names are listed in the review and recorded in
-- the confirmation's audit row. `flags` is unchanged.
--
-- Additive and defaulted: a row written before this column reads as a
-- proposal that left no field off, which is what those rows were.
ALTER TABLE pricing_import_proposals
  ADD COLUMN dropped jsonb NOT NULL DEFAULT '[]';
