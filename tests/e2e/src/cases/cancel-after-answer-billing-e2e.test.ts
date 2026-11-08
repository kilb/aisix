import { createHash, randomUUID } from "node:crypto";
import { createServer, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  startMockSls,
  waitConfigPropagation,
  waitForSlsLog,
  type MockSls,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: a caller that hangs up after the upstream answered is billed the
// answer.
//
// The upstream charges for an answer the moment it sends it. When an output
// guardrail is still reading that answer, the caller is still waiting, and a
// caller that gives up then (a client timeout, a closed tab) leaves the
// gateway holding an answer the platform has already paid for. The usage row
// for that request must bill what the upstream charged, exactly as the row
// of a delivered answer would, or every slow output scan is a way to get
// answers billed at nothing.
//
// Every surface a client SDK uses is driven, buffered and streamed: Chat
// Completions, Messages translated onto a Chat Completions upstream, Messages
// relayed to a native Anthropic upstream, and Responses. Each cell has its
// own model. The output guardrail is a moderation service that answers at
// once, except for a text carrying the slow marker, which it holds; the
// caller hangs up once the service has that text.
//
// Every upstream answer reports 500 prompt tokens, 100 of them read from the
// prompt cache, and 300 completion tokens, so a delivered answer and an
// abandoned one must both bill exactly that.

const CALLER = "sk-cancel-after-answer";
const CREDENTIAL_REF = "mock";
const SLS_PROJECT = "aisix-e2e-obs";
const LOGSTORE = "cancel-after-answer";
const SLOW = "slow-scan-marker";
const PROMPT = 500;
const CACHED = 100;
const COMPLETION = 300;
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
}

const SHAPES: Array<[Endpoint, Upstream]> = [
  ["chat", "openai"],
  ["messages", "openai"],
  ["messages", "anthropic"],
  ["responses", "openai"],
];

const CELLS: Cell[] = SHAPES.flatMap(([endpoint, upstream]) =>
  [false, true].map((stream) => ({
    name: `cancel-paid-${endpoint}-${upstream}-${stream ? "stream" : "buffered"}`,
    endpoint,
    upstream,
    stream,
  })),
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

/** An upstream that answers every request by repeating its prompt, in the
 * protocol of the path it was sent, reporting the usage above. */
async function startUpstream(): Promise<{ baseUrl: string; close(): Promise<void> }> {
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
        const usage = {
          prompt_tokens: PROMPT,
          completion_tokens: COMPLETION,
          total_tokens: PROMPT + COMPLETION,
          prompt_tokens_details: { cached_tokens: CACHED },
        };
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
        const usage = {
          input_tokens: PROMPT,
          output_tokens: COMPLETION,
          total_tokens: PROMPT + COMPLETION,
          input_tokens_details: { cached_tokens: CACHED },
        };
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
            usage: {
              input_tokens: PROMPT,
              output_tokens: COMPLETION,
              cache_read_input_tokens: CACHED,
            },
          });
        }
        return sse(res, [
          ["message_start", { type: "message_start", message: { id, type: "message", role: "assistant", model, content: [], stop_reason: null, usage: { input_tokens: PROMPT, output_tokens: 1, cache_read_input_tokens: CACHED } } }],
          ["content_block_start", { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }],
          ["content_block_delta", { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: answer } }],
          ["content_block_stop", { type: "content_block_stop", index: 0 }],
          ["message_delta", { type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: COMPLETION } }],
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

/** A moderation service that flags nothing, and holds any text carrying the
 * slow marker without answering. */
async function startModeration(): Promise<{
  baseUrl: string;
  inputs: string[];
  close(): Promise<void>;
}> {
  const inputs: string[] = [];
  const server: Server = createServer((req, res) => {
    res.on("error", () => {});
    let raw = "";
    req.on("data", (c: Buffer) => (raw += c.toString("utf8")));
    req.on("end", () => {
      let input = "";
      try {
        const body = JSON.parse(raw) as Json;
        input = typeof body.input === "string" ? body.input : JSON.stringify(body.input ?? "");
      } catch {
        input = raw;
      }
      inputs.push(input);
      if (input.includes(SLOW)) return; // held until the server closes
      res.statusCode = 200;
      res.setHeader("content-type", "application/json");
      res.end(
        JSON.stringify({
          id: "modr-mock",
          model: "omni-moderation-latest",
          results: [{ flagged: false, categories: {}, category_scores: {} }],
        }),
      );
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    inputs,
    close: async () => {
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

describe("a caller that hangs up after the upstream answered is billed the answer", () => {
  let app: SpawnedApp | undefined;
  let upstream: Awaited<ReturnType<typeof startUpstream>> | undefined;
  let moderation: Awaited<ReturnType<typeof startModeration>> | undefined;
  let sls: MockSls | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    upstream = await startUpstream();
    moderation = await startModeration();
    sls = await startMockSls();
    app = await spawnApp({
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-akid",
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: "sls-cancel-after-answer",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: SLS_PROJECT,
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "metadata_only",
    });

    const openai = await seed.createProviderKey({
      display_name: "cancel-paid-openai",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    const anthropic = await seed.createProviderKey({
      display_name: "cancel-paid-anthropic",
      provider: "anthropic",
      adapter: "anthropic",
      secret: "sk-ant-mock",
      api_base: upstream.baseUrl,
    });
    const scan = await seed.createGuardrail(
      {
        name: "cancel-paid-output-scan",
        enabled: true,
        kind: "openai_moderation",
        hook_point: "output",
        api_key: "sk-moderation",
        endpoint: moderation.baseUrl,
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
      await seed.attachGuardrailToModel(scan.id as string, model.id as string);
    }
    // The key last: once it authenticates, the rest is in the snapshot.
    await seed.createApiKey({ key_hash: hash(CALLER), allowed_models: ["*"] });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER}` },
      });
      return res.status === 200;
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
    await moderation?.close();
    await sls?.close();
  });

  const send = (cell: Cell, text: string, signal?: AbortSignal) => {
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
    return fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER}`,
        "x-api-key": CALLER,
        "anthropic-version": "2023-06-01",
        "content-type": "application/json",
      },
      body: JSON.stringify(body),
      signal,
    });
  };

  const rowOf = (cell: Cell, status: string) =>
    waitForSlsLog(
      sls!,
      LOGSTORE,
      (log) => log.get("requested_model") === cell.name && log.get("status_code") === status,
      `a ${status} usage row for ${cell.name}`,
      20_000,
    );

  /** What a row bills, as the control plane prices it. */
  const BILLED = [
    "model_id",
    "prompt_tokens",
    "completion_tokens",
    "total_tokens",
    "cached_prompt_tokens",
    "cache_read_tokens",
    "cache_creation_tokens",
    "cost_usd",
  ];

  for (const cell of CELLS) {
    test(`${cell.name}: the abandoned answer is billed as the delivered one is`, async (ctx) => {
      if (!etcdReachable) ctx.skip();

      // A delivered answer, for the row it bills.
      const clean = await send(cell, "hello");
      const cleanBody = await clean.text();
      expect(clean.status, cleanBody).toBe(200);
      expect(cleanBody).toContain("you said: hello");
      const delivered = await rowOf(cell, "200");
      expect(delivered.get("prompt_tokens")).toBe(String(PROMPT));
      expect(delivered.get("completion_tokens")).toBe(String(COMPLETION));
      expect(Number(delivered.get("cost_usd") ?? "0")).toBeGreaterThan(0);

      // The same answer, with the caller hanging up while the output scan
      // still holds it.
      const marker = `${SLOW}-${cell.name}`;
      const controller = new AbortController();
      const abandoned = send(cell, `please repeat ${marker}`, controller.signal).then((res) =>
        res.text(),
      );
      for (let i = 0; i < 400 && !moderation!.inputs.some((t) => t.includes(marker)); i++) {
        await new Promise((r) => setTimeout(r, 25));
      }
      expect(
        moderation!.inputs.some((t) => t.includes(marker)),
        "the upstream's answer never reached the output scan",
      ).toBe(true);
      controller.abort();
      await expect(abandoned).rejects.toThrow();

      const row = await rowOf(cell, "499");
      expect(row.get("error_class")).toBe("client_disconnected");
      // The upstream charged for the answer; the row bills it in the
      // upstream's own shape: OpenAI's cache hit inside the prompt,
      // Anthropic's cache read beside it.
      expect(row.get("prompt_tokens")).toBe(String(PROMPT));
      expect(row.get("completion_tokens")).toBe(String(COMPLETION));
      if (cell.upstream === "anthropic") {
        expect(row.get("cache_read_tokens")).toBe(String(CACHED));
      } else {
        expect(row.get("cached_prompt_tokens")).toBe(String(CACHED));
      }
      // And exactly as the delivered answer's row bills it.
      const bill = (log: Map<string, string>) =>
        Object.fromEntries(BILLED.map((field) => [field, log.get(field) ?? ""]));
      expect(bill(row)).toEqual(bill(delivered));
    });
  }
});
