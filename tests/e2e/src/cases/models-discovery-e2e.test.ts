import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";

// Model discovery is the first call most SDKs make, and it was served in one
// dialect only. An Anthropic SDK doing `client.models.list()` against the
// gateway received OpenAI's `{object: "list", data: [...]}` envelope and
// failed to deserialize — a break that surfaces only on the client, which is
// the same silent shape as every other endpoint-family gap in this repo.
//
// Both SDK families define list AND retrieve at the same two paths, so one
// pair of routes serves both and the request's `anthropic-version` header
// picks the shape.
//
// `created` is asserted to be the epoch and, more importantly, STABLE: it
// used to be stamped with the request time, so the same catalogue differed
// on every call and any client diffing or caching it saw churn that was not
// there.

const CALLER = "sk-models-discovery-e2e";
const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");

interface OpenAiList {
  object?: string;
  data?: Array<{ id?: string; object?: string; created?: number; owned_by?: string }>;
}
interface AnthropicList {
  object?: string;
  data?: Array<{ id?: string; type?: string; display_name?: string; created_at?: string }>;
  first_id?: string | null;
  last_id?: string | null;
  has_more?: boolean;
}

describe("model discovery in both dialects", () => {
  let app: SpawnedApp | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "md-pk",
      secret: "sk-mock",
      api_base: "http://127.0.0.1:9/v1",
    });
    for (const name of ["md-alpha", "md-bravo", "md-charlie"]) {
      await seed.createModel({
        display_name: name,
        provider: "openai",
        model_name: "gpt-4o-mini",
        provider_key_id: pk.id,
      });
    }
    // A SECOND row carrying an existing display name. Two rows may share
    // one: the id index holds both while the name index keeps only the last.
    // Listed twice, it advertises a duplicate id and wedges the cursor —
    // `after_id=md-bravo` resolves to the first occurrence, so the page after
    // it repeats forever and the pagination walk below never terminates.
    await seed.createModel({
      display_name: "md-bravo",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    // A model this key may NOT reach, to pin the ACL and the 404 fold.
    await seed.createModel({
      display_name: "md-secret",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    // Last, so a gate on this key authenticating implies the rows above.
    await seed.createApiKey({
      key_hash: sha256(CALLER),
      allowed_models: ["md-alpha", "md-bravo", "md-charlie"],
    });
  });

  afterAll(async () => {
    await app?.exit();
  });

  test("each SDK dialect gets its own envelope from one route", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    await waitConfigPropagation(async () => {
      const r = await get("/v1/models");
      if (r.status !== 200) return false;
      return ((await r.json()) as OpenAiList).data?.length === 3;
    });

    const openai = (await (await get("/v1/models")).json()) as OpenAiList;
    expect(openai.object).toBe("list");
    expect(openai.data?.map((m) => m.id)).toEqual(["md-alpha", "md-bravo", "md-charlie"]);
    expect(
      openai.data?.map((m) => m.id),
      "the list must not advertise a model this key cannot then use",
    ).not.toContain("md-secret");

    const anthropic = (await (
      await get("/v1/models", { "anthropic-version": "2023-06-01" })
    ).json()) as AnthropicList;
    expect(
      anthropic.object,
      "OpenAI's envelope key must be absent, or an Anthropic SDK fails to parse",
    ).toBeUndefined();
    expect(anthropic.has_more).toBe(false);
    expect(anthropic.first_id).toBe("md-alpha");
    expect(anthropic.last_id).toBe("md-charlie");
    expect(anthropic.data?.[0]?.type).toBe("model");
    expect(anthropic.data?.[0]?.display_name).toBe("md-alpha");
    expect(anthropic.data?.[0]?.created_at).toBe("1970-01-01T00:00:00Z");
  }, 60_000);

  test("`created` is a constant, so the catalogue cannot churn", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    // Asserted as the exact constant rather than by comparing two responses.
    // Comparing them looks like the stronger test and is in fact vacuous:
    // the old code stamped `created` with the request time in WHOLE SECONDS,
    // so two calls milliseconds apart matched anyway — which is also why the
    // churn was easy to miss in the first place. The value being a constant
    // is what actually makes the catalogue stable across a second boundary,
    // a restart, or a second replica.
    const list = (await (await get("/v1/models")).json()) as OpenAiList;
    expect(list.data?.map((m) => m.created)).toEqual([0, 0, 0]);
  }, 60_000);

  test("a paginating client walks every model once and stops", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    const seen: string[] = [];
    let cursor: string | undefined;
    for (let i = 0; i < 10; i++) {
      const path = cursor
        ? `/v1/models?limit=1&after_id=${cursor}`
        : "/v1/models?limit=1";
      const page = (await (
        await get(path, { "anthropic-version": "2023-06-01" })
      ).json()) as AnthropicList;
      expect(page.data?.length).toBe(1);
      seen.push(page.data![0].id!);
      if (!page.has_more) break;
      cursor = page.last_id!;
    }
    expect(seen, "the walk must terminate having seen each model once").toEqual([
      "md-alpha",
      "md-bravo",
      "md-charlie",
    ]);
  }, 60_000);

  test("a name carried by two rows is listed once", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    const list = (await (await get("/v1/models")).json()) as OpenAiList;
    const ids = list.data!.map((m) => m.id);
    expect(ids, "the catalogue is a set of names, not a row dump").toEqual([
      "md-alpha",
      "md-bravo",
      "md-charlie",
    ]);
  }, 60_000);

  test("a malformed `limit` comes back in the caller's own error shape", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    // A typed extractor rejects this before the handler runs, and the
    // framework's rejection is plain text. Neither SDK can parse that, so
    // the client raises a decode error instead of surfacing the 400 it was
    // actually sent — the same client-side-only break as the wrong list
    // envelope.
    for (const dialect of [{}, { "anthropic-version": "2023-06-01" }]) {
      for (const qs of ["limit=abc", "limit=0", "limit=99999"]) {
        const r = await get(`/v1/models?${qs}`, dialect);
        expect(r.status, `${qs} ${JSON.stringify(dialect)}`).toBe(400);
        const body = await r.json();
        expect(body?.error?.type, `${qs} kept its error envelope`).toBeTruthy();
      }
    }
  }, 60_000);

  test("retrieve serves both dialects, and hides what the key may not reach", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    const o = await get("/v1/models/md-alpha");
    expect(o.status).toBe(200);
    expect((await o.json()).object).toBe("model");

    const a = await get("/v1/models/md-alpha", { "anthropic-version": "2023-06-01" });
    expect(a.status).toBe(200);
    const body = await a.json();
    expect(body.type).toBe("model");
    expect(body.display_name).toBe("md-alpha");

    // A model that exists but is out of this key's reach must be
    // indistinguishable from one that does not exist — otherwise the error
    // code alone enumerates the environment.
    const forbidden = await get("/v1/models/md-secret");
    const missing = await get("/v1/models/md-nothing");
    expect([forbidden.status, missing.status]).toEqual([404, 404]);
  }, 60_000);

  function get(path: string, extra: Record<string, string> = {}) {
    return fetch(`${app!.proxyUrl}${path}`, {
      headers: { authorization: `Bearer ${CALLER}`, ...extra },
    });
  }
});
