import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError, resources, type ResourceView } from "./api";

const WATCH_INTERVAL_MS = 1_000;
const WATCH_BUDGET_MS = 30_000;

function asApiError(e: unknown): ApiError {
  return e instanceof ApiError
    ? e
    : new ApiError(e instanceof Error ? e.message : String(e), 0, "client", [], [], null);
}

/** Waits `ms`, or less when `signal` aborts first. */
function pause(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal.aborted) return resolve();
    const done = () => {
      clearTimeout(timer);
      signal.removeEventListener("abort", done);
      resolve();
    };
    const timer = setTimeout(done, ms);
    signal.addEventListener("abort", done);
  });
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
 * - `watch(id, signal?)` re-reads the row every second for up to 30 s while
 *   its rollout is `pending` or `propagating`, updating `items` with each
 *   view it reads, and resolves to the last good view. A failed read once a
 *   view has been read does not end the watch: it keeps polling until the
 *   budget runs out. Only a failure before any view rejects. `signal`
 *   aborting, or the page unmounting, stops the watch and aborts the read
 *   in flight; it then resolves to the last good view, or rejects with the
 *   abort's error when there is none.
 */
export function useResource<D = Record<string, unknown>>(kind: string) {
  const [items, setItems] = useState<ResourceView<D>[] | null>(null);
  const [error, setError] = useState<ApiError | null>(null);
  // Aborted when the page that uses the hook unmounts.
  const lifetime = useRef<AbortController | null>(null);
  useEffect(() => {
    const controller = new AbortController();
    lifetime.current = controller;
    return () => controller.abort();
  }, []);
  const alive = () => !lifetime.current?.signal.aborted;

  const reload = useCallback(async () => {
    try {
      const list = await resources.list<D>(kind);
      if (!alive()) return;
      setItems(list);
      setError(null);
    } catch (e) {
      if (alive()) setError(asApiError(e));
    }
  }, [kind]);

  useEffect(() => {
    setItems(null);
    void reload();
  }, [reload]);

  const upsert = useCallback((view: ResourceView<D>) => {
    if (!alive()) return;
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
      if (alive()) setItems((prev) => prev?.filter((i) => i.id !== id) ?? null);
      return view;
    },
    [kind],
  );

  const watch = useCallback(
    async (id: string, signal?: AbortSignal): Promise<ResourceView<D>> => {
      // One signal for "stop": the caller's, or the page unmounting.
      const stop = new AbortController();
      const onAbort = () => stop.abort();
      for (const s of [signal, lifetime.current?.signal]) {
        if (s?.aborted) stop.abort();
        else s?.addEventListener("abort", onAbort, { once: true });
      }
      const deadline = Date.now() + WATCH_BUDGET_MS;
      let last: ResourceView<D> | null = null;
      try {
        for (;;) {
          try {
            const view = await resources.get<D>(kind, id, stop.signal);
            upsert(view);
            last = view;
            if (view.rollout.state !== "pending" && view.rollout.state !== "propagating") {
              return view;
            }
          } catch (e) {
            if (last === null) throw e;
            // A failed read after a good one: keep polling on the last view.
          }
          if (stop.signal.aborted || Date.now() + WATCH_INTERVAL_MS > deadline) return last;
          await pause(WATCH_INTERVAL_MS, stop.signal);
        }
      } finally {
        signal?.removeEventListener("abort", onAbort);
        lifetime.current?.signal.removeEventListener("abort", onAbort);
      }
    },
    [kind, upsert],
  );

  return { items, error, reload, create, save, remove, watch };
}
