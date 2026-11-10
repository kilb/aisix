import { createHash, randomUUID } from "node:crypto";
import { createServer, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: a key limited to one request at a time stays at one while the output
// guardrail is still reading an answer.
//
// `rate_limit.concurrency` caps the requests a key has in flight. A request
// whose answer an output guardrail is still checking is in flight: the caller
// is waiting for it. So a second request from the same key must be refused
// while the first one's answer is in the check, whether that answer came from
// the upstream or from the cache, and served once the first one is done.
//
// The output guardrail is a moderation service that answers at once, except
// for a text carrying the armed marker, which it holds until the test lets it
// go. Each cell has its own key and its own model: a fresh Chat Completions
// answer, and a cache hit on Chat Completions, Messages and Responses.

const hash = (s: string) => createHash("sha256").update(s).digest("hex");

type Json = Record<string, unknown>;
type Endpoint = "chat" | "messages" | "responses";

interface Cell {
  name: string;
  endpoint: Endpoint;
  cached: boolean;
  key: string;
}

const CELLS: Cell[] = [
  { name: "held-fresh-chat", endpoint: "chat", cached: false },
  { name: "held-hit-chat", endpoint: "chat", cached: true },
  { name: "held-hit-messages", endpoint: "messages", cached: true },
  { name: "held-hit-responses", endpoint: "responses", cached: true },
].map((c) => ({ ...c, endpoint: c.endpoint as Endpoint, key: `sk-${c.name}-e2e` }));

/** The text of the last user turn, whichever protocol carried it. */
function promptOf(body: Json): string {
  if (typeof body.input === "string") return body.input;
  const turns = (body.messages ?? []) as Json[];
  const user = [...turns].reverse().find((t) => t.role === "user");
  const content = user?.content;
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .map((part) => (part as Json)?.text)
      .filter((t): t is string => typeof t === "string")
      .join("");
  }
  return "";
}

/** An upstream that answers every request by repeating its prompt. */
async function startUpstream(): Promise<{ baseUrl: string; calls: string[]; close(): Promise<void> }> {
  const calls: string[] = [];
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
      const answer = `you said: ${promptOf(body)}`;
      calls.push(answer);
      res.statusCode = 200;
      res.setHeader("content-type", "application/json");
      if (path === "/v1/responses") {
        return res.end(
          JSON.stringify({
            id: `resp_${randomUUID()}`,
            object: "response",
            created_at: 0,
            model: String(body.model ?? ""),
            status: "completed",
            output: [
              {
                type: "message",
                id: "msg_1",
                role: "assistant",
                status: "completed",
                content: [{ type: "output_text", text: answer, annotations: [] }],
              },
            ],
            usage: { input_tokens: 5, output_tokens: 3, total_tokens: 8 },
          }),
        );
      }
      return res.end(
        JSON.stringify({
          id: `chatcmpl-${randomUUID()}`,
          object: "chat.completion",
          created: 0,
          model: String(body.model ?? ""),
          choices: [{ index: 0, message: { role: "assistant", content: answer }, finish_reason: "stop" }],
          usage: { prompt_tokens: 5, completion_tokens: 3, total_tokens: 8 },
        }),
      );
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    calls,
    close: async () => {
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

/** A moderation service that flags nothing, and holds any text carrying the
 * armed marker until it is released. */
async function startModeration() {
  const held: ServerResponse[] = [];
  const state = { armed: "", heldCount: 0 };
  const answer = (res: ServerResponse) => {
    res.statusCode = 200;
    res.setHeader("content-type", "application/json");
    res.end(
      JSON.stringify({
        id: "modr-mock",
        model: "omni-moderation-latest",
        results: [{ flagged: false, categories: {}, category_scores: {} }],
      }),
    );
  };
  const server: Server = createServer((req, res) => {
    res.on("error", () => {});
    let raw = "";
    req.on("data", (c: Buffer) => (raw += c.toString("utf8")));
    req.on("end", () => {
      if (state.armed && raw.includes(state.armed)) {
        held.push(res);
        state.heldCount += 1;
        return;
      }
      answer(res);
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    state,
    release() {
      state.armed = "";
      for (const res of held.splice(0)) answer(res);
    },
    close: async () => {
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

describe("a key at its concurrency limit stays there while its answer is checked", () => {
  let app: SpawnedApp | undefined;
  let upstream: Awaited<ReturnType<typeof startUpstream>> | undefined;
  let moderation: Awaited<ReturnType<typeof startModeration>> | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    upstream = await startUpstream();
    moderation = await startModeration();
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "held-openai",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    const scan = await seed.createGuardrail(
      {
        name: "held-output-scan",
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
        provider: "openai",
        model_name: "gpt-4o-mini",
        provider_key_id: pk.id,
      });
      await seed.attachGuardrailToModel(scan.id as string, model.id as string);
      if (cell.cached) {
        await seed.createCachePolicy({
          name: `${cell.name}-cache`,
          enabled: true,
          backend: "memory",
          ttl_seconds: 300,
          applies_to: `model:${cell.name}`,
        });
      }
    }
    // The keys last: once the last one authenticates, the rest is in.
    for (const cell of CELLS) {
      await seed.createApiKey({
        key_hash: hash(cell.key),
        allowed_models: [cell.name],
        rate_limit: { concurrency: 1 },
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
    moderation?.release();
    await app?.exit();
    await upstream?.close();
    await moderation?.close();
  });

  const send = (cell: Cell, text: string) => {
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
    return fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${cell.key}`,
        "x-api-key": cell.key,
        "anthropic-version": "2023-06-01",
        "content-type": "application/json",
      },
      body: JSON.stringify(body),
    });
  };

  const until = async (cond: () => boolean, what: string) => {
    const deadline = Date.now() + 10_000;
    while (!cond()) {
      if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
      await new Promise((r) => setTimeout(r, 20));
    }
  };

  for (const cell of CELLS) {
    test(`${cell.name}: a second request waits out the first one's output check`, async (ctx) => {
      if (!etcdReachable) ctx.skip();
      const marker = `held-${randomUUID()}`;
      const prompt = `please repeat ${marker}`;

      if (cell.cached) {
        // Store the answer, and see the next identical request served from
        // the cache: it does not reach the upstream.
        const stored = await send(cell, prompt);
        expect(stored.status, await stored.text()).toBe(200);
        const before = upstream!.calls.length;
        const again = await send(cell, prompt);
        expect(again.status, await again.text()).toBe(200);
        expect(upstream!.calls.length).toBe(before);
      }

      moderation!.state.armed = marker;
      const heldBefore = moderation!.state.heldCount;
      const first = send(cell, prompt);
      await until(() => moderation!.state.heldCount > heldBefore, "the first answer to be in the check");

      // The first request is still in flight, so its key is at its limit.
      const second = await send(cell, `another question ${randomUUID()}`);
      const secondBody = await second.text();
      expect(second.status, secondBody).toBe(429);

      moderation!.release();
      const done = await first;
      const doneBody = await done.text();
      expect(done.status, doneBody).toBe(200);
      expect(doneBody).toContain(marker);

      // Done, so the slot is free again.
      const third = await send(cell, `a third question ${randomUUID()}`);
      expect(third.status, await third.text()).toBe(200);
    });
  }
});
