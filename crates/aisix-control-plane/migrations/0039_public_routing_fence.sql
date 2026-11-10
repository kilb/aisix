-- Fences the writers that predate 0037 out of a public model's routing
-- document.
--
-- Since 0037 a public model's routing settings live in
-- `public_models.routing`, and the control plane generates the routing
-- document (the `models` row whose id is the public model's and whose owner
-- is `public:<id>`) from them and from `channel_models`. A control plane
-- older than 0037 opens a database migrated further (`Db::migrate` ignores
-- the migrations it does not know) and, on a channel edit, writes the same
-- document from its own columns: the default strategy and none of the
-- settings. The gateway would then route the model as if the operator had
-- set nothing, while the stored settings say otherwise.
--
-- So a live routing document written for a public model must carry every
-- setting the public model stores, each key present with the stored value
-- (jsonb equality: an array keeps its order, a 0 is not an absent key). The
-- write that does not is refused, and that older control plane's channel
-- edit fails instead of silently reverting the settings. A control plane at
-- or after 0037 always writes the stored settings, a public model that
-- stores none (`{}`) is written as before by every release, and a delete
-- (a tombstone) is not judged. Like 0027's fences this is one-way: while an
-- older control plane runs beside a newer one, its channel edits of a model
-- with stored settings answer an error.
CREATE FUNCTION public_routing_kept() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
  stored jsonb;
BEGIN
  IF NEW.kind <> 'models' OR NEW.deleted
     OR NEW.managed_by IS DISTINCT FROM 'public:' || NEW.id::text THEN
    RETURN NEW;
  END IF;
  SELECT p.routing INTO stored FROM public_models p WHERE p.id = NEW.id;
  IF stored IS NULL OR jsonb_typeof(stored) <> 'object' THEN
    RETURN NEW;
  END IF;
  IF EXISTS (
    SELECT 1 FROM jsonb_each(stored) AS s(key, value)
    WHERE (NEW.doc -> 'routing' -> s.key) IS DISTINCT FROM s.value
  ) THEN
    RAISE EXCEPTION 'the routing document of public model % does not carry its stored routing settings', NEW.id
      USING ERRCODE = 'check_violation', CONSTRAINT = 'public_routing_kept';
  END IF;
  RETURN NEW;
END
$$;

CREATE TRIGGER public_routing_kept
  BEFORE INSERT OR UPDATE ON gw_resources
  FOR EACH ROW EXECUTE FUNCTION public_routing_kept();
