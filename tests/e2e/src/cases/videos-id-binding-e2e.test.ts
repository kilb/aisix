import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: a video id reaches only the model it was minted for.
//
// User journey: a caller whose key may use `video-basic` (and the
// `wan/turbo` alias of a `wan/*` wildcard model) submits generation tasks
// and polls them by the returned id. The id is opaque but not secret: it
// decodes (base64url) to `<model entry id>:<base64url(model name)>:<task id>`,
// so a caller can re-point it at another model's entry while keeping the
// model name its key is allowed to use. The same gateway also serves
// `video-premium`, a model that key may NOT use. A re-pointed id must not
// reach `video-premium`'s upstream: it is an id the gateway never minted,
// so it is answered like any unknown id (404), and the premium upstream
// sees no request at all.

const CALLER = "sk-video-binding-caller";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");

// A DashScope-shaped task envelope: submit and poll both answer with it.
const pendingTask = (taskId: string) => ({
  nonStreamBody: { output: { task_id: taskId, task_status: "PENDING" }, request_id: `req-${taskId}` },
});

describe("a video id reaches only the model it was minted for", () => {
  let app: SpawnedApp | undefined;
  let basic: OpenAiUpstream | undefined;
  let premium: OpenAiUpstream | undefined;
  let wild: OpenAiUpstream | undefined;
  let premiumEntryId = "";
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    basic = await startOpenAiUpstream(pendingTask("task-basic-1"));
    premium = await startOpenAiUpstream(pendingTask("task-premium-1"));
    wild = await startOpenAiUpstream(pendingTask("task-wild-1"));
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

    for (const [name, upstream] of [
      ["video-basic", basic],
      ["video-premium", premium],
      ["wan/*", wild],
    ] as const) {
      const pk = await seed.createProviderKey({
        display_name: `${name.replace("/*", "-wild")}-pk`,
        provider: "alibaba",
        secret: "sk-mock-dashscope",
        api_base: upstream.baseUrl,
      });
      const model = await seed.createModel({
        display_name: name,
        provider: "alibaba",
        model_name: "wan-mock",
        provider_key_id: pk.id,
      });
      if (name === "video-premium") premiumEntryId = model.id;
    }

    // Caller key last: once it authenticates, every resource above is in
    // the snapshot.
    await seed.createApiKey({ key_hash: CALLER_HASH, allowed_models: ["video-basic", "wan/turbo"] });
    const proxy = new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await proxy.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    await basic?.close();
    await premium?.close();
    await wild?.close();
  });

  const auth = { authorization: `Bearer ${CALLER}` };

  async function submit(model: string): Promise<string> {
    const res = await fetch(`${app!.proxyUrl}/v1/videos`, {
      method: "POST",
      headers: { ...auth, "content-type": "application/json" },
      body: JSON.stringify({ model, prompt: "a cardboard city at night" }),
    });
    const text = await res.text();
    expect(res.status, text).toBe(200);
    return (JSON.parse(text) as { id: string }).id;
  }

  const get = (id: string, suffix = "") =>
    fetch(`${app!.proxyUrl}/v1/videos/${id}${suffix}`, { headers: auth, redirect: "manual" });

  /** The same id with its model entry segment swapped for `entryId`. */
  function repoint(id: string, entryId: string): string {
    const [, ...rest] = Buffer.from(id, "base64url").toString("utf8").split(":");
    return Buffer.from([entryId, ...rest].join(":")).toString("base64url");
  }

  test("a key polls the task it submitted", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const id = await submit("video-basic");
    const before = basic!.receivedRequests.length;
    const res = await get(id);
    const text = await res.text();
    expect(res.status, text).toBe(200);
    expect((JSON.parse(text) as { id: string }).id).toBe(id);
    expect(basic!.receivedRequests.length).toBe(before + 1);
  });

  test("a key allowed for a wildcard model's alias polls the task it submitted", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const id = await submit("wan/turbo");
    const res = await get(id);
    expect(res.status, await res.text()).toBe(200);
  });

  test("an id re-pointed at a model the key may not use is not found, and that model is never called", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    for (const model of ["video-basic", "wan/turbo"]) {
      const forged = repoint(await submit(model), premiumEntryId);
      for (const suffix of ["", "/content"]) {
        const res = await get(forged, suffix);
        expect(res.status, `${model}${suffix}: ${await res.text()}`).toBe(404);
      }
    }
    expect(premium!.receivedRequests).toHaveLength(0);
  });
});
