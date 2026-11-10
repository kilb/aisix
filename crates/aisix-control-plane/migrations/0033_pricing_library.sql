-- The price library: a stored copy of the public price dataset the import
-- reads (0026), from which an operator syncs baselines. It is filled either
-- by fetching the dataset from its one published address or by an
-- operator's upload of the dataset file, and a newer copy replaces the older
-- in the same transaction, so the table holds at most the copy in use.
--
-- `data` is the dataset's own text, never `jsonb`: every price is read from
-- its decimal text, and `jsonb` would rewrite a number (`1.5e-05` becomes
-- `0.000015`), turning a refused price into a readable one.
--
-- Nothing here is a price. A sync, which an operator attests, writes a
-- `third_party` baseline (0025) naming this row's id.
CREATE TABLE pricing_library (
  id uuid PRIMARY KEY,
  dataset text NOT NULL,
  origin text NOT NULL CHECK (origin IN ('fetch', 'upload')),
  -- The address a fetch read; NULL for an upload.
  source_url text,
  -- The revision the payload names, or the one the operator gave an upload.
  source_ref text,
  sha256 text NOT NULL,
  bytes bigint NOT NULL,
  -- How many entries it holds: each model once, cardless ones included.
  entries bigint NOT NULL,
  data text NOT NULL,
  fetched_at timestamptz NOT NULL DEFAULT now(),
  created_by uuid NOT NULL REFERENCES operators(id));
