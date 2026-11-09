import { createHash, randomUUID } from "node:crypto";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { WebSocket as WsClient, WebSocketServer } from "ws";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  metricDelta,
  scrapeMetrics,
  SeedClient,
  slsLogsFor,
  spawnApp,
  startMockSls,
  waitConfigPropagation,
  waitForSlsLog,
  type MetricSample,
  type MockSls,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: what a request costs, who pays it, and what a cache may hand back, read
// from the outside: the `cost_usd` and `api_key_id` on the usage row each
// request leaves with an exporter, the status a caller sees, and whether the
// upstream was reached.
//
// Every model below is priced in round numbers. 1000 tokens at 1 USD/1k is one
// dollar, so an expected value is arithmetic on the numbers in this file, read
// here in micro-USD (one dollar is 1_000_000).

const COST = { input_per_1k: 1, output_per_1k: 1 };
const CREDENTIAL_REF = "mock";
const LOGSTORE = "money-accounting";

/** A row's `cost_usd`, in micro-USD. */
const microOf = (row: Map<string, string>) => Math.round(Number(row.get("cost_usd") ?? "0") * 1e6);

const hash = (s: string) => createHash("sha256").update(s).digest("hex");

type Json = Record<string, unknown>;

interface Batch {
  id: string;
  object: "batch";
  status: string;
  endpoint: string;
  input_file_id: string;
  output_file_id?: string;
  metadata?: unknown;
}

interface Mock {
  baseUrl: string;
  /** Times each upstream model name was asked for, per path. */
  hits: Map<string, number>;
  batches: Map<string, Batch>;
  close(): Promise<void>;
}

const hitsOf = (mock: Mock, path: string, model: string) =>
  mock.hits.get(`${path} ${model}`) ?? 0;

const chatAnswer = (model: string, message: Json, usage: Json = {}) => ({
  id: `chatcmpl-${randomUUID()}`,
  object: "chat.completion",
  created: 0,
  model,
  choices: [{ index: 0, message: { role: "assistant", ...message }, finish_reason: "stop" }],
  usage: { prompt_tokens: 1000, completion_tokens: 1000, total_tokens: 2000, ...usage },
});

const responseAnswer = (model: string, extra: Json = {}) => ({
  id: `resp_${randomUUID()}`,
  object: "response",
  created_at: 0,
  model,
  status: "completed",
  output: [
    {
      type: "message",
      id: "msg_1",
      role: "assistant",
      status: "completed",
      content: [{ type: "output_text", text: `from ${model}`, annotations: [] }],
    },
  ],
  usage: { input_tokens: 1000, output_tokens: 1000, total_tokens: 2000 },
  ...extra,
});

const messageAnswer = (model: string, usage: Json = {}) => ({
  id: `msg_${randomUUID()}`,
  type: "message",
  role: "assistant",
  model,
  content: [{ type: "text", text: `from ${model}` }],
  stop_reason: "end_turn",
  usage: { input_tokens: 1000, output_tokens: 1000, ...usage },
});

/** One upstream for every surface. It behaves as a provider does: a batch keeps
 * the metadata it was created with, and returns it on every read. */
async function startMock(): Promise<Mock> {
  const hits = new Map<string, number>();
  const batches = new Map<string, Batch>();
  const server: Server = createServer((req, res) => {
    res.on("error", () => {});
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => {
      const raw = Buffer.concat(chunks);
      const path = (req.url ?? "/").split("?")[0];
      let body: Json = {};
      if ((req.headers["content-type"] ?? "").includes("json")) {
        try {
          body = JSON.parse(raw.toString("utf8")) as Json;
        } catch {
          body = {};
        }
      }
      const model = typeof body.model === "string" ? body.model : "";
      const key = `${path} ${model}`;
      hits.set(key, (hits.get(key) ?? 0) + 1);
      const json = (status: number, payload: unknown) => {
        res.statusCode = status;
        res.setHeader("content-type", "application/json");
        res.end(JSON.stringify(payload));
      };

      if (path === "/v1/chat/completions") {
        switch (model) {
          case "refuser":
            return json(200, chatAnswer(model, { content: null, refusal: "I can't help with that." }));
          case "cache-writer":
            return json(
              200,
              chatAnswer(model, { content: "ok" }, {
                prompt_tokens_details: { cached_tokens: 100, cache_write_tokens: 700 },
              }),
            );
          default:
            return json(200, chatAnswer(model, { content: `from ${model}` }));
        }
      }
      if (path === "/v1/responses") {
        if (model === "failer") {
          return json(
            200,
            responseAnswer(model, {
              status: "failed",
              output: [],
              error: { code: "server_error", message: "the model failed" },
            }),
          );
        }
        return json(200, responseAnswer(model));
      }
      if (path === "/v1/messages") {
        switch (model) {
          case "cache-only":
            return json(
              200,
              messageAnswer(model, { input_tokens: 0, cache_read_input_tokens: 1000 }),
            );
          case "searcher":
            return json(
              200,
              messageAnswer(model, { server_tool_use: { web_search_requests: 2 } }),
            );
          default:
            return json(200, messageAnswer(model));
        }
      }
      if (path === "/v1/images/generations") {
        return json(200, { created: 0, data: [{ url: "https://images.example/1.png" }] });
      }
      if (req.method === "POST" && path === "/v1/files") {
        return json(200, {
          id: "file-in",
          object: "file",
          purpose: "batch",
          filename: "in.jsonl",
          bytes: raw.length,
        });
      }
      if (req.method === "GET" && /^\/v1\/files\/[^/]+\/content$/.test(path)) {
        res.statusCode = 200;
        res.setHeader("content-type", "application/jsonl");
        return res.end(
          `${JSON.stringify({
            id: "req_1",
            custom_id: "r1",
            response: {
              status_code: 200,
              body: { usage: { prompt_tokens: 1000, completion_tokens: 1000 } },
            },
          })}\n`,
        );
      }
      if (req.method === "POST" && path === "/v1/batches") {
        const id = `batch_${randomUUID()}`;
        const batch: Batch = {
          id,
          object: "batch",
          status: "validating",
          endpoint: String(body.endpoint),
          input_file_id: String(body.input_file_id),
          metadata: body.metadata ?? null,
        };
        batches.set(id, batch);
        return json(200, batch);
      }
      if (req.method === "GET" && path === "/v1/batches") {
        return json(200, { object: "list", data: [...batches.values()] });
      }
      const batchPath = /^\/v1\/batches\/([^/]+)(\/cancel)?$/.exec(path);
      if (batchPath) {
        const batch = batches.get(batchPath[1]);
        if (!batch) return json(404, { error: { message: "no such batch" } });
        if (batchPath[2]) batch.status = "cancelling";
        return json(200, batch);
      }
      return json(404, { error: { message: `no route for ${path}` } });
    });
  });

  // A realtime session: one `response.done` for each thing the client asks.
  const realtime = new WebSocketServer({ server });
  realtime.on("connection", (socket) => {
    socket.on("message", () => {
      socket.send(
        JSON.stringify({
          type: "response.done",
          response: { usage: { input_tokens: 1000, output_tokens: 1000 } },
        }),
      );
    });
  });

  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    hits,
    batches,
    close: async () => {
      for (const client of realtime.clients) client.terminate();
      realtime.close();
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

const CALLER = "sk-money-caller";
const SUBMITTER = "sk-money-submitter";
const SUBMITTER_ID = randomUUID();
const READER = "sk-money-reader";
const READER_ID = randomUUID();
const BUDGET = "sk-money-budget";
const BUDGET_ID = randomUUID();
const TOKENS = "sk-money-tokens";
const TOKENS_ID = randomUUID();
const SESSION = "sk-money-session";
const SESSION_ID = randomUUID();

describe("what a request costs, who pays it, and what a cache hands back", () => {
  let app: SpawnedApp | undefined;
  let mock: Mock | undefined;
  let sls: MockSls | undefined;
  let seed: SeedClient | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    mock = await startMock();
    sls = await startMockSls();
    // The loopback client is a trusted proxy, so a test can say where a request
    // comes from.
    app = await spawnApp({
      realIp: { trusted_proxies: ["127.0.0.1/32"], recursive: true },
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-akid",
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-secret",
      },
    });
    seed = new SeedClient(etcd, app.etcdPrefix);
    // Every request's usage row, as an exporter keeps it.
    await seed.createObservabilityExporter({
      name: "sls-money-accounting",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: "aisix-e2e-obs",
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "metadata_only",
    });

    const openai = await seed.createProviderKey({
      display_name: "money-openai",
      secret: "sk-mock",
      api_base: `${mock.baseUrl}/v1`,
    });
    const anthropic = await seed.createProviderKey({
      display_name: "money-anthropic",
      provider: "anthropic",
      adapter: "anthropic",
      secret: "sk-ant-mock",
      api_base: mock.baseUrl,
    });
    const model = (display: string, extra: Json = {}, pk = openai.id, provider = "openai") =>
      seed!.createModel({
        display_name: display,
        provider,
        model_name: display,
        provider_key_id: pk,
        cost: COST,
        ...extra,
      });

    await model("cache-writer", {
      cost: { ...COST, cached_input_per_1k: 0.1, cache_write_per_1k: 1.25 },
    });
    await model("cache-only", { cost: { ...COST, cached_input_per_1k: 0.1 } }, anthropic.id, "anthropic");
    await model("refuser");
    await model("failer");
    await model("repeatable");
    await model("tag-eu");
    await model("tag-us");
    await seed.createModel({
      display_name: "tag-router",
      routing: {
        strategy: "failover",
        targets: [
          { model: "tag-eu", tags: ["eu"] },
          { model: "tag-us", tags: ["us"] },
        ],
      },
    });
    // Only this network may reach the member the group dispatches to.
    await model("inside-only", { allowed_cidrs: ["203.0.113.0/24"] });
    await seed.createModel({
      display_name: "inside-group",
      routing: { strategy: "failover", targets: [{ model: "inside-only" }] },
    });
    await model("inside-only-claude", { allowed_cidrs: ["203.0.113.0/24"] }, anthropic.id, "anthropic");
    await seed.createModel({
      display_name: "inside-group-claude",
      routing: { strategy: "failover", targets: [{ model: "inside-only-claude" }] },
    });
    await seed.createCachePolicy({ name: "money-cache", enabled: true, applies_to: "all" });

    // A check that costs money per call, and blocks on a phrase.
    const checked = await model("checked");
    const rule = await seed.createGuardrail(
      {
        name: "money-remote-check",
        enabled: true,
        hook_point: "input",
        fail_open: false,
        kind: "custom",
        timeout_ms: 5000,
        script: `export function checkInput(ctx) {
          return ctx.text.includes("forbidden")
            ? { action: "block", reason_code: "phrase" }
            : { action: "none" };
        }`,
      },
      { attach: false },
    );
    await seed.attachGuardrailToModel(rule.id, checked.id);

    await model("painter");
    await model("searcher", {}, anthropic.id, "anthropic");
    await model("batcher");
    await model("speaker");

    await seed.createPassthroughRoute({
      name: "money-tunnel",
      path_prefix: "/tunnel",
      target_url: mock.baseUrl,
      provider_key_id: openai.id,
    });

    await seed.update("budgets", randomUUID(), {
      scope: "api_key",
      scope_ref: BUDGET_ID,
      period: "month",
      exhausted: { reason: { message: "monthly cap reached" } },
    });
    await seed.createRateLimitPolicy({
      name: "money-tunnel-tokens",
      scope: "api_key",
      scope_ref: TOKENS_ID,
      window: "day",
      max_tokens: 50,
    });

    // Keys last: once the last one authenticates, the rest is in the snapshot.
    const putKey = (id: string, plaintext: string) =>
      seed!.update("api_keys", id, {
        key_hash: hash(plaintext),
        allowed_models: ["*"],
        allowed_routes: ["*"],
      });
    await putKey(BUDGET_ID, BUDGET);
    await putKey(TOKENS_ID, TOKENS);
    await putKey(SUBMITTER_ID, SUBMITTER);
    await putKey(READER_ID, READER);
    await putKey(SESSION_ID, SESSION);
    await seed.createApiKey({
      key_hash: hash(CALLER),
      allowed_models: ["*"],
      allowed_routes: ["*"],
    });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER}` },
      });
      return res.status === 200;
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await mock?.close();
    await sls?.close();
  });

  const scrape = () => scrapeMetrics(app!.metricsUrl);

  const post = (path: string, key: string, body: Json, headers: Record<string, string> = {}) =>
    fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${key}`,
        "x-api-key": key,
        "anthropic-version": "2023-06-01",
        "content-type": "application/json",
        ...headers,
      },
      body: JSON.stringify(body),
    });

  const chat = (model: string, text: string, key = CALLER, headers: Record<string, string> = {}) =>
    post("/v1/chat/completions", key, { model, messages: [{ role: "user", content: text }] }, headers);

  const responses = (model: string, text: string, headers: Record<string, string> = {}) =>
    post("/v1/responses", CALLER, { model, input: text }, headers);

  const messages = (model: string, text: string, key = CALLER, headers: Record<string, string> = {}) =>
    post(
      "/v1/messages",
      key,
      { model, max_tokens: 16, messages: [{ role: "user", content: text }] },
      headers,
    );

  /** The usage row of the request `res` answered. */
  const rowOf = (res: Response) => {
    const requestId = res.headers.get("x-aisix-request-id") ?? "";
    expect(requestId, "the response names its request").not.toBe("");
    return waitForSlsLog(
      sls!,
      LOGSTORE,
      (log) => log.get("request_id") === requestId,
      `the usage row of ${requestId}`,
      20_000,
    );
  };

  /** Every usage row received so far that `pred` matches. */
  const rowsWhere = (pred: (row: Map<string, string>) => boolean) =>
    slsLogsFor(sls!, LOGSTORE).filter(pred);

  /** Wait until `pred` matches at least `n` usage rows, and return them all. */
  async function awaitRows(
    pred: (row: Map<string, string>) => boolean,
    n: number,
    what: string,
  ): Promise<Map<string, string>[]> {
    for (let i = 0; i < 200; i++) {
      const rows = rowsWhere(pred);
      if (rows.length >= n) return rows;
      await new Promise((r) => setTimeout(r, 100));
    }
    throw new Error(`fewer than ${n} usage rows: ${what}`);
  }

  /** Wait for a series to move from `before`, then read how far. */
  async function deltaOf(
    before: MetricSample[],
    name: string,
    labels: Record<string, string>,
  ): Promise<number> {
    let got = 0;
    for (let i = 0; i < 60; i++) {
      got = metricDelta(before, await scrape(), name, labels);
      if (got !== 0) return got;
      await new Promise((r) => setTimeout(r, 50));
    }
    return got;
  }

  const settle = () => new Promise((r) => setTimeout(r, 300));

  test("a cache write is charged at the write rate, inside a prompt that reports it", async (ctx) => {
    if (!etcdReachable) return ctx.skip();
    const res = await chat("cache-writer", "price this");
    expect(res.status).toBe(200);
    // Of 1000 prompt tokens: 100 read at 0.1, 700 written at 1.25, 200 plain at
    // 1. Plus 1000 completion tokens at 1.
    expect(microOf(await rowOf(res))).toBe(2_085_000);
  });

  test("a prompt served entirely from the cache is charged at the cache rate alone", async (ctx) => {
    if (!etcdReachable) return ctx.skip();
    const res = await messages("cache-only", "a long and cached prompt");
    expect(res.status).toBe(200);
    // 1000 read at 0.1, 1000 completion at 1. An input count of zero beside
    // those reads is what the provider reported, and nothing is added to it.
    expect(microOf(await rowOf(res))).toBe(1_100_000);
  });

  test("an answer worth repeating is cached, so the cases below are not vacuous", async (ctx) => {
    if (!etcdReachable) return ctx.skip();
    for (let i = 0; i < 2; i++) {
      expect((await chat("repeatable", "again")).status).toBe(200);
      expect((await responses("repeatable", "again")).status).toBe(200);
      await settle();
    }
    expect(hitsOf(mock!, "/v1/chat/completions", "repeatable")).toBe(1);
    expect(hitsOf(mock!, "/v1/responses", "repeatable")).toBe(1);
  });

  test("a refusal and a failed response are not handed to the next caller", async (ctx) => {
    if (!etcdReachable) return ctx.skip();
    for (let i = 0; i < 2; i++) {
      expect((await chat("refuser", "ask")).status).toBe(200);
      await responses("failer", "ask");
      await settle();
    }
    expect(hitsOf(mock!, "/v1/chat/completions", "refuser")).toBe(2);
    expect(hitsOf(mock!, "/v1/responses", "failer")).toBe(2);
  });

  test("requests tagged for different targets do not share an answer", async (ctx) => {
    if (!etcdReachable) return ctx.skip();
    const ask = async (tag: string, call: "chat" | "responses") => {
      const res =
        call === "chat"
          ? await chat("tag-router", "where", CALLER, { "x-aisix-routing-tags": tag })
          : await responses("tag-router", "where", { "x-aisix-routing-tags": tag });
      expect(res.status).toBe(200);
      const text = await res.text();
      await settle();
      return text;
    };
    for (const call of ["chat", "responses"] as const) {
      expect(await ask("eu", call), call).toContain("from tag-eu");
      expect(await ask("us", call), call).toContain("from tag-us");
      // The same tag again is the same question, and is a hit.
      const path = call === "chat" ? "/v1/chat/completions" : "/v1/responses";
      const eu = hitsOf(mock!, path, "tag-eu");
      expect(await ask("eu", call), call).toContain("from tag-eu");
      expect(hitsOf(mock!, path, "tag-eu"), call).toBe(eu);
    }
  });

  test("a cached answer still answers to a member's network allowlist", async (ctx) => {
    if (!etcdReachable) return ctx.skip();
    const inside = { "x-forwarded-for": "203.0.113.9" };
    const outside = { "x-forwarded-for": "198.51.100.9" };

    expect((await responses("inside-group", "secret", inside)).status).toBe(200);
    await settle();
    expect((await responses("inside-group", "secret", inside)).status).toBe(200);
    expect(hitsOf(mock!, "/v1/responses", "inside-only"), "the second one is a hit").toBe(1);
    const refused = await responses("inside-group", "secret", outside);
    expect(refused.status).toBe(403);
    expect(await refused.text()).not.toContain("from inside-only");

    expect((await messages("inside-group-claude", "secret", CALLER, inside)).status).toBe(200);
    await settle();
    const refusedMessage = await messages("inside-group-claude", "secret", CALLER, outside);
    expect(refusedMessage.status).toBe(403);
    expect(await refusedMessage.text()).not.toContain("from inside-only-claude");
  });

  test("an exhausted budget refuses before a paid check runs", async (ctx) => {
    if (!etcdReachable) return ctx.skip();
    // The rule really does block this phrase, for a caller with money left.
    expect((await chat("checked", "forbidden", CALLER)).status).toBe(422);

    expect((await chat("checked", "forbidden", BUDGET)).status).toBe(429);
  });

  test("an identified passthrough exchange counts its tokens against the caller's limit", async (ctx) => {
    if (!etcdReachable) return ctx.skip();
    const call = (text: string) =>
      post("/tunnel/v1/chat/completions", TOKENS, {
        model: "upstream-model",
        messages: [{ role: "user", content: text }],
      });
    expect((await call("first")).status).toBe(200);
    let status = 0;
    for (let i = 0; i < 40; i++) {
      status = (await call(`next ${i}`)).status;
      if (status === 429) break;
      await new Promise((r) => setTimeout(r, 50));
    }
    expect(status, "2000 tokens went through a limit of 50").toBe(429);
  });

  async function submitBatch(key: string): Promise<{ id: string; raw: string }> {
    const form = new FormData();
    form.set("purpose", "batch");
    form.set("file", new Blob(['{"custom_id":"r1"}\n']), "in.jsonl");
    const file = await fetch(`${app!.proxyUrl}/v1/files`, {
      method: "POST",
      headers: { authorization: `Bearer ${key}`, "x-aisix-model": "batcher" },
      body: form,
    });
    expect(file.status).toBe(200);
    const { id: fileId } = (await file.json()) as { id: string };
    const created = await post("/v1/batches", key, {
      input_file_id: fileId,
      endpoint: "/v1/chat/completions",
      completion_window: "24h",
    });
    expect(created.status).toBe(200);
    const { id } = (await created.json()) as { id: string };
    const raws = [...mock!.batches.keys()];
    return { id, raw: raws[raws.length - 1] };
  }

  test("a batch is charged to whoever submitted it, once, whoever reads it", async (ctx) => {
    if (!etcdReachable) return ctx.skip();
    const batchRow = (row: Map<string, string>) =>
      row.get("is_batch") === "true" && row.get("requested_model") === "batcher";
    const earlier = rowsWhere(batchRow).length;
    const { id, raw } = await submitBatch(SUBMITTER);
    const batch = mock!.batches.get(raw)!;
    batch.status = "completed";
    batch.output_file_id = `file-out-${raw}`;

    // Someone else's listing is the first time anyone sees it finish.
    const listed = await fetch(`${app!.proxyUrl}/v1/batches?model=batcher`, {
      headers: { authorization: `Bearer ${READER}` },
    });
    expect(listed.status).toBe(200);
    const [charged] = (await awaitRows(batchRow, earlier + 1, "the batch's row")).slice(earlier);
    expect(charged.get("api_key_id"), "the submitter pays, not the reader").toBe(SUBMITTER_ID);
    expect(microOf(charged)).toBe(2_000_000);

    // The submitter reading it afterwards is not a second charge.
    const read = await fetch(`${app!.proxyUrl}/v1/batches/${id}`, {
      headers: { authorization: `Bearer ${SUBMITTER}` },
    });
    expect(read.status).toBe(200);
    // Ordered, not timed: a request after the read has its row exported after
    // any the read produced.
    const barrier = await chat("repeatable", "after the read", SUBMITTER);
    expect(barrier.status).toBe(200);
    await rowOf(barrier);
    expect(rowsWhere(batchRow).slice(earlier), "one row for the batch").toHaveLength(1);
  });

  test("a batch cancelled with finished work is charged for that work", async (ctx) => {
    if (!etcdReachable) return ctx.skip();
    const batchRow = (row: Map<string, string>) =>
      row.get("is_batch") === "true" && row.get("api_key_id") === SUBMITTER_ID;
    const earlier = rowsWhere(batchRow).length;
    const { id, raw } = await submitBatch(SUBMITTER);
    const cancel = await post(`/v1/batches/${id}/cancel`, SUBMITTER, {});
    expect(cancel.status).toBe(200);

    const batch = mock!.batches.get(raw)!;
    batch.status = "cancelled";
    batch.output_file_id = `file-out-${raw}`;

    const read = await fetch(`${app!.proxyUrl}/v1/batches/${id}`, {
      headers: { authorization: `Bearer ${SUBMITTER}` },
    });
    expect(read.status).toBe(200);
    const [charged] = (await awaitRows(batchRow, earlier + 1, "the cancelled batch's row")).slice(
      earlier,
    );
    expect(charged.get("requested_model")).toBe("batcher");
    expect(microOf(charged)).toBe(2_000_000);
  });

  test("an open realtime session is counted as it goes, and ends when its key does", async (ctx) => {
    if (!etcdReachable) return ctx.skip();
    const before = await scrape();
    const session = new WsClient(
      `${app!.proxyUrl.replace("http", "ws")}/v1/realtime?model=speaker`,
      { headers: { authorization: `Bearer ${SESSION}` } },
    );
    const frames: string[] = [];
    const closed = new Promise<number>((resolve) => {
      session.on("close", (code) => resolve(code));
    });
    session.on("message", (data: Buffer) => frames.push(data.toString("utf8")));
    await new Promise<void>((resolve, reject) => {
      session.once("open", () => resolve());
      session.once("error", reject);
    });

    session.send(JSON.stringify({ type: "response.create" }));
    expect(
      await deltaOf(before, "aisix_llm_input_tokens_total", {
        api_key_id: SESSION_ID,
        model: "speaker",
      }),
      "counted while the session is still open",
    ).toBe(1000);

    await seed!.update("api_keys", SESSION_ID, {
      key_hash: hash(SESSION),
      allowed_models: ["*"],
      allowed_routes: ["*"],
      disabled: true,
    });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${SESSION}` },
      });
      return res.status === 401;
    });

    session.send(JSON.stringify({ type: "response.create" }));
    expect(await closed).toBe(1008);
    expect(frames, "the response already under way was delivered").toHaveLength(2);
    // The session's one row bills both responses to the key that opened it.
    const [row] = await awaitRows(
      (r) => r.get("api_key_id") === SESSION_ID && r.get("requested_model") === "speaker",
      1,
      "the realtime session's row",
    );
    expect(row.get("prompt_tokens")).toBe("2000");
    expect(microOf(row)).toBe(4_000_000);
  });
});
