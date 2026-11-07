import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError, resources, type ResourceView } from "./api";

const WATCH_INTERVAL_MS = 1_000;
const WATCH_BUDGET_MS = 30_000;

function asApiError(e: unknown): ApiError {
  return e instanceof ApiError
    ? e
    : new ApiError(e instanceof Error ? e.message : String(e), 0, "client", [], [], null);
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

/**
 * The rows of one writable kind (the plural, such as `provider_keys`) and
 * the writes on them.
 *
 * - `items` is null until the first read; `error` is the last failed read.
 * - `create`, `save` and `remove` throw the `ApiError` of a refused write
 *   (a form shows it); a successful one updates `items` from the answer.
 *   `save` and `remove` send `If-Match` with the version the edit started
 *   from, and are never retried: a 412 is the caller's to show.
 * - `watch(id)` re-reads the row every second for up to 30 s while its
 *   rollout is `pending` or `propagating`, updating `items` each time, and
 *   resolves to the last view it read.
 */
export function useResource<D = Record<string, unknown>>(kind: string) {
  const [items, setItems] = useState<ResourceView<D>[] | null>(null);
  const [error, setError] = useState<ApiError | null>(null);
  // Polls stop once the page that started them is gone.
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  const reload = useCallback(async () => {
    try {
      const list = await resources.list<D>(kind);
      if (!alive.current) return;
      setItems(list);
      setError(null);
    } catch (e) {
      if (alive.current) setError(asApiError(e));
    }
  }, [kind]);

  useEffect(() => {
    setItems(null);
    void reload();
  }, [reload]);

  const upsert = useCallback((view: ResourceView<D>) => {
    if (!alive.current) return;
    setItems((prev) => {
      const list = prev ?? [];
      return list.some((i) => i.id === view.id)
        ? list.map((i) => (i.id === view.id ? view : i))
        : [...list, view];
    });
  }, []);

  const create = useCallback(
    async (doc: D) => {
      const view = await resources.create<D>(kind, doc);
      upsert(view);
      return view;
    },
    [kind, upsert],
  );

  const save = useCallback(
    async (id: string, doc: D, version: number) => {
      const view = await resources.save<D>(kind, id, doc, version);
      upsert(view);
      return view;
    },
    [kind, upsert],
  );

  const remove = useCallback(
    async (id: string, version: number) => {
      const view = await resources.remove<D>(kind, id, version);
      if (alive.current) setItems((prev) => prev?.filter((i) => i.id !== id) ?? null);
      return view;
    },
    [kind],
  );

  const watch = useCallback(
    async (id: string) => {
      const deadline = Date.now() + WATCH_BUDGET_MS;
      for (;;) {
        const view = await resources.get<D>(kind, id);
        upsert(view);
        const moving = view.rollout.state === "pending" || view.rollout.state === "propagating";
        if (!moving || !alive.current || Date.now() + WATCH_INTERVAL_MS > deadline) return view;
        await sleep(WATCH_INTERVAL_MS);
      }
    },
    [kind, upsert],
  );

  return { items, error, reload, create, save, remove, watch };
}
