import { createHash, randomUUID } from "node:crypto";
import { createServer, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  metricDelta,
  scrapeMetrics,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  waitForLogLine,
  type MetricSample,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: an answer an OUTPUT guardrail blocks is paid for like one it lets
// through.
//
// The output check runs after the upstream has answered, so the provider has
// already charged for those tokens: to the operator a blocked answer costs
// exactly what a delivered one does. Both sides of the gateway's books must
// see it. The spend series has to count it, or the money is spent and never
// recorded; and the caller's spend ceiling has to count it, or a caller who
// keeps tripping the guardrail spends the operator's upstream money without
// limit.
//
// Every surface a client SDK uses is driven, buffered and streamed, because
// each records spend on its own path: Chat Completions, Messages translated
// from a Chat Completions upstream, Messages relayed from a native Anthropic
// upstream, and Responses. Each cell has its own key, its own model and its
// own day-window ceiling of two answers.
//
// Every model is priced at 1 USD per 1k tokens and every upstream answer
// reports 500 + 500 tokens, so one answer is 1_000_000 on the spend series.

const SPEND = "aisix_llm_spend_micro_usd_total";
const ONE_ANSWER = 1_000_000;
const MARKER = "bill-guard-marker";
const COST = { input_per_1k: 1, output_per_1k: 1 };

const hash = (s: string) => createHash("sha256").update(s).digest("hex");

type Json = Record<string, unknown>;
type Endpoint = "chat" | "messages" | "responses";
type Upstream = "openai" | "anthropic";

interface Cell {
  name: string;
  endpoint: Endpoint;
  upstream: Upstream;
  stream: boolean;
  keyId: string;
  key: string;
}

const SHAPES: Array<[Endpoint, Upstream]> = [
  ["chat", "openai"],
  ["messages", "openai"],
  ["messages", "anthropic"],
  ["responses", "openai"],
];

const CELLS: Cell[] = SHAPES.flatMap(([endpoint, upstream]) =>
  [false, true].map((stream) => {
    const name = `bill-${endpoint}-${upstream}-${stream ? "stream" : "buffered"}`;
    return { name, endpoint, upstream, stream, keyId: randomUUID(), key: `sk-${name}` };
  }),
);

/** The text of the last user turn, whichever protocol carried it. */
function promptOf(body: Json): string {
  const text = (content: unknown): string =>
    typeof content === "string"
      ? content
      : Array.isArray(content)
        ? content
            .map((part) => (part as Json)?.text)
            .filter((t): t is string => typeof t === "string")
            .join("")
        : "";
  if (typeof body.input === "string") return body.input;
  const turns = (body.messages ?? body.input) as Json[] | undefined;
  const user = [...(turns ?? [])].reverse().find((t) => t.role === "user");
  return text(user?.content);
}

function sse(res: ServerResponse, events: Array<[string | null, unknown]>) {
  res.statusCode = 200;
  res.setHeader("content-type", "text/event-stream");
  for (const [event, data] of events) {
    if (event) res.write(`event: ${event}\n`);
    res.write(`data: ${typeof data === "string" ? data : JSON.stringify(data)}\n\n`);
  }
  res.end();
}

/** An upstream that answers every request by repeating its prompt, priced at
 * 500 input and 500 output tokens, in the protocol of the path it was sent. */
async function startMock(): Promise<{ baseUrl: string; close(): Promise<void> }> {
  const server: Server = createServer((req, res) => {
    res.on("error", () => {});
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => {
      const path = (req.url ?? "/").split("?")[0];
      let body: Json = {};
      try {
        body = JSON.parse(Buffer.concat(chunks).toString("utf8")) as Json;
      } catch {
        body = {};
      }
      const model = String(body.model ?? "");
      const answer = `you said: ${promptOf(body)}`;
      const stream = body.stream === true;
      const json = (payload: unknown) => {
        res.statusCode = 200;
        res.setHeader("content-type", "application/json");
        res.end(JSON.stringify(payload));
      };

      if (path === "/v1/chat/completions") {
        const id = `chatcmpl-${randomUUID()}`;
        const usage = { prompt_tokens: 500, completion_tokens: 500, total_tokens: 1000 };
        if (!stream) {
          return json({
            id,
            object: "chat.completion",
            created: 0,
            model,
            choices: [
              { index: 0, message: { role: "assistant", content: answer }, finish_reason: "stop" },
            ],
            usage,
          });
        }
        const chunk = (delta: Json, finish: string | null, extra: Json = {}) => ({
          id,
          object: "chat.completion.chunk",
          created: 0,
          model,
          choices: [{ index: 0, delta, finish_reason: finish }],
          ...extra,
        });
        return sse(res, [
          [null, chunk({ role: "assistant" }, null)],
          [null, chunk({ content: answer }, null)],
          [null, chunk({}, "stop", { usage })],
          [null, "[DONE]"],
        ]);
      }

      if (path === "/v1/responses") {
        const id = `resp_${randomUUID()}`;
        const usage = { input_tokens: 500, output_tokens: 500, total_tokens: 1000 };
        const item = {
          type: "message",
          id: "msg_1",
          role: "assistant",
          status: "completed",
          content: [{ type: "output_text", text: answer, annotations: [] }],
        };
        const response = {
          id,
          object: "response",
          created_at: 0,
          model,
          status: "completed",
          output: [item],
          usage,
        };
        if (!stream) return json(response);
        return sse(res, [
          ["response.created", { type: "response.created", response: { ...response, status: "in_progress", output: [], usage: null } }],
          ["response.output_text.delta", { type: "response.output_text.delta", item_id: "msg_1", output_index: 0, content_index: 0, delta: answer }],
          ["response.completed", { type: "response.completed", response }],
        ]);
      }

      if (path === "/v1/messages") {
        const id = `msg_${randomUUID()}`;
        if (!stream) {
          return json({
            id,
            type: "message",
            role: "assistant",
            model,
            content: [{ type: "text", text: answer }],
            stop_reason: "end_turn",
            usage: { input_tokens: 500, output_tokens: 500 },
          });
        }
        return sse(res, [
          ["message_start", { type: "message_start", message: { id, type: "message", role: "assistant", model, content: [], stop_reason: null, usage: { input_tokens: 500, output_tokens: 1 } } }],
          ["content_block_start", { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }],
          ["content_block_delta", { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: answer } }],
          ["content_block_stop", { type: "content_block_stop", index: 0 }],
          ["message_delta", { type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 500 } }],
          ["message_stop", { type: "message_stop" }],
        ]);
      }

      res.statusCode = 404;
      res.end(JSON.stringify({ error: { message: `no route for ${path}` } }));
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    close: async () => {
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

describe("an answer an output guardrail blocks is billed like one it lets through", () => {
  let app: SpawnedApp | undefined;
  let mock: Awaited<ReturnType<typeof startMock>> | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    mock = await startMock();
    // The access-log line is `info`; the barrier below waits for one.
    app = await spawnApp({ logLevel: "info" });
    const seed = new SeedClient(etcd, app.etcdPrefix);

    const openai = await seed.createProviderKey({
      display_name: "bill-openai",
      secret: "sk-mock",
      api_base: `${mock.baseUrl}/v1`,
    });
    const anthropic = await seed.createProviderKey({
      display_name: "bill-anthropic",
      provider: "anthropic",
      adapter: "anthropic",
      secret: "sk-ant-mock",
      api_base: mock.baseUrl,
    });
    const rule = await seed.createGuardrail(
      {
        name: "bill-output-block",
        enabled: true,
        kind: "custom",
        hook_point: "output",
        output_fail_open: false,
        timeout_ms: 5000,
        script: `export function checkOutput(ctx) {
          return ctx.text.includes("${MARKER}") ? { action: "block" } : { action: "none" };
        }`,
      },
      { attach: false },
    );

    for (const cell of CELLS) {
      const model = await seed.createModel({
        display_name: cell.name,
        provider: cell.upstream,
        model_name: cell.upstream === "anthropic" ? "claude-3-5-haiku-20241022" : "gpt-4o-mini",
        provider_key_id: cell.upstream === "anthropic" ? anthropic.id : openai.id,
        cost: COST,
      });
      await seed.attachGuardrailToModel(rule.id as string, model.id as string);
      // Room for two answers in the day: the warm-up and the blocked one.
      await seed.createRateLimitPolicy({
        name: `${cell.name}-ceiling`,
        scope: "api_key",
        scope_ref: cell.keyId,
        window: "day",
        max_spend_micro_usd: 2 * ONE_ANSWER,
      });
    }

    // Keys last: once the last one authenticates, the rest is in the snapshot.
    for (const cell of CELLS) {
      await seed.update("api_keys", cell.keyId, {
        key_hash: hash(cell.key),
        allowed_models: [cell.name],
      });
    }
    const last = CELLS[CELLS.length - 1];
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${last.key}` },
      });
      return res.status === 200;
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await mock?.close();
  });

  const send = async (cell: Cell, text: string) => {
    const path = {
      chat: "/v1/chat/completions",
      messages: "/v1/messages",
      responses: "/v1/responses",
    }[cell.endpoint];
    const body: Json =
      cell.endpoint === "responses"
        ? { model: cell.name, input: text }
        : cell.endpoint === "messages"
          ? { model: cell.name, max_tokens: 64, messages: [{ role: "user", content: text }] }
          : { model: cell.name, messages: [{ role: "user", content: text }] };
    if (cell.stream) body.stream = true;
    const res = await fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${cell.key}`,
        "x-api-key": cell.key,
        "anthropic-version": "2023-06-01",
        "content-type": "application/json",
      },
      body: JSON.stringify(body),
    });
    return {
      status: res.status,
      body: await res.text(),
      requestId: res.headers.get("x-aisix-request-id") ?? "",
    };
  };

  const spendSince = async (cell: Cell, before: MetricSample[]) =>
    metricDelta(before, await scrapeMetrics(app!.metricsUrl), SPEND, { model: cell.name });

  for (const cell of CELLS) {
    test(`${cell.name}: the blocked answer is billed and counts against the ceiling`, async (ctx) => {
      if (!etcdReachable) ctx.skip();
      const start = await scrapeMetrics(app!.metricsUrl);

      // An answer the guardrail lets through, billed as one answer. A stream
      // records its spend as it ends, which can follow the caller's last byte.
      const clean = await send(cell, "hello");
      expect(clean.status, clean.body).toBe(200);
      expect(clean.body).toContain("you said: hello");
      await expect.poll(() => spendSince(cell, start), { timeout: 10_000 }).toBe(ONE_ANSWER);

      // An answer the guardrail blocks: the caller does not get it.
      const before = await scrapeMetrics(app!.metricsUrl);
      const blocked = await send(cell, `please repeat ${MARKER}`);
      if (cell.stream) {
        // A stream may be refused with a status or with an error event.
        expect([200, 422], blocked.body).toContain(blocked.status);
        if (blocked.status === 200) expect(blocked.body).toContain("error");
      } else {
        expect(blocked.status, blocked.body).toBe(422);
      }
      expect(blocked.body, "the blocked answer never reaches the caller").not.toContain(
        `you said: please repeat ${MARKER}`,
      );

      // The upstream charged for it, so the spend series counts it.
      await expect
        .poll(() => spendSince(cell, before), {
          timeout: 10_000,
          message: "the blocked answer was paid upstream but never recorded as spend",
        })
        .toBeGreaterThan(0);

      // And the ceiling counts it: two answers spent its whole day, so the
      // next request is refused before it reaches the upstream.
      const refused = await send(cell, `please repeat ${MARKER}`);
      expect(
        refused.status,
        `a blocked answer did not count against the caller's ceiling: ${refused.body}`,
      ).toBe(429);

      // Once, and the refused request not at all. The barrier is ordered,
      // not timed: the refused request went out only after the blocked
      // answer reached the caller, and its own access-log line is written
      // once that request is done with, so the check below runs after both
      // requests have finished.
      expect(refused.requestId, "the refused request has an id").not.toBe("");
      await waitForLogLine(
        app!,
        (l) =>
          l.includes("proxy request completed") &&
          l.includes(`request_id="${refused.requestId}"`),
        `the access-log line for the refused request ${refused.requestId}`,
      );
      expect(
        await spendSince(cell, before),
        "the blocked answer is billed once and the refused request not at all",
      ).toBe(ONE_ANSWER);
    });
  }
});
