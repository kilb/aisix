import { createHash } from "node:crypto";
import { createServer, type IncomingMessage, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: Responses conversations that go on past their first turn in platform
// mode, where one gateway and one set of upstream accounts serve many
// tenants.
//
// - A code interpreter ran in a container the upstream keeps. The stateless
//   way to go on sends the earlier output back, the call's `container_id`
//   with it, and the upstream reuses that container. The tenant it was
//   handed to goes on; anyone else naming that container, or the upstream's
//   own id for it, is refused before any upstream hears of it.
// - An output guardrail that can block holds a stream back until it has read
//   all of it. What it then releases names no upstream id, exactly as a live
//   stream does, and the ids it hands out work on the next turn.
// - Behind a routing group, a reasoning item's encrypted content can only be
//   read by the account that wrote it. A tenant's next turn must reach that
//   account with the ciphertext it wrote, and another tenant echoing it must
//   not get it read anywhere.
//
// The upstream below stands in for every account: the bearer credential
// names the account, and every id it hands out carries a channel marker
// (`bdrk`) and the account's name, which no caller may see.

const hash = (plaintext: string) => createHash("sha256").update(plaintext).digest("hex");
const PLATFORM_SECRET_ENV = "P0_E2E_CONTINUATIONS_SECRET";
// 32 random bytes as hex digits, the form the gateway requires.
const PLATFORM_SECRET = "5d0c3f8a7e21b94c6a1d2e3f405162738495a6b7c8d9e0f1a2b3c4d5e6f70819";
const KEYS = {
  /** Tenant A. */
  a: "sk-platform-continuations-a",
  /** Tenant B: another team on the same gateway and accounts. */
  b: "sk-platform-continuations-b",
};
const MODELS = {
  interpreter: "pc-interpreter",
  held: "pc-held",
  groupA: "pc-group-a",
  groupB: "pc-group-b",
  group: "pc-group",
};
/** A word no answer contains: the output guardrail never blocks. */
const NEVER_SAID = "zq-never-in-any-answer-7k";
/** What only the upstream's own ids carry. */
const CHANNEL_MARKER = "bdrk";
/**
 * The opaque ciphertext an account writes for a reasoning item: only that
 * account can read it, and it names no account in the clear.
 */
const ciphertext = (account: string, n: number) =>
  `enc:${createHash("sha256").update(account).digest("hex").slice(0, 12)}:${n}`;
const ciphertextOf = (account: string) => new RegExp(`^${ciphertext(account, 0).slice(0, -1)}\\d+$`);

interface Received {
  account: string;
  body: any;
  raw: string;
}

const RESPONSES_TOOLS = [
  {
    type: "function",
    name: "get_weather",
    parameters: { type: "object", properties: { city: { type: "string" } } },
  },
];
const INTERPRETER_TOOLS = [{ type: "code_interpreter", container: { type: "auto" } }];

/** One HTTP server for every upstream account; the bearer names the account. */
class Accounts {
  readonly received: Received[] = [];
  /** Every Response the accounts answered with, in order. */
  readonly answered: Array<Record<string, any>> = [];
  private seq = 0;
  /** The containers each account ran, by id. */
  private readonly containers = new Map<string, string>();
  private constructor(private readonly server: Server) {}

  static async start(): Promise<Accounts> {
    let self: Accounts | undefined;
    const server = createServer((req, res) => {
      void self!.handle(req).then(({ status, body, sse }) => {
        if (sse !== undefined) {
          res.writeHead(status, { "content-type": "text/event-stream" });
          res.end(sse);
        } else {
          res.writeHead(status, { "content-type": "application/json" });
          res.end(JSON.stringify(body));
        }
      });
    });
    self = new Accounts(server);
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    return self;
  }

  get baseUrl(): string {
    return `http://127.0.0.1:${(this.server.address() as AddressInfo).port}/v1`;
  }

  close(): Promise<void> {
    return new Promise((resolve) => this.server.close(() => resolve()));
  }

  last(): Received {
    const last = this.received.at(-1);
    expect(last, "a request reached the upstream").toBeDefined();
    return last!;
  }

  private async handle(req: IncomingMessage): Promise<{ status: number; body?: unknown; sse?: string }> {
    let raw = "";
    for await (const chunk of req) raw += chunk;
    const account = (req.headers.authorization ?? "").replace(/^Bearer /, "");
    const body = raw ? JSON.parse(raw) : {};
    this.received.push({ account, body, raw });
    const refuse = (message: string, code: string | null = null) => ({
      status: 400,
      body: { error: { message, type: "invalid_request_error", param: null, code } },
    });

    // What only this account can read, and the containers only it ran.
    let container: string | undefined;
    for (const item of body.input ?? []) {
      if (typeof item?.encrypted_content === "string" && !ciphertextOf(account).test(item.encrypted_content)) {
        return refuse("The encrypted content could not be verified.", "invalid_encrypted_content");
      }
      if (item?.type === "code_interpreter_call") {
        if (this.containers.get(item.container_id) !== account) {
          return refuse(`Container with id '${item.container_id}' not found.`);
        }
        container = item.container_id;
      }
    }

    const n = ++this.seq;
    const tag = `${CHANNEL_MARKER}_${account}_${n}`;
    const output: Array<Record<string, unknown>> = [
      {
        type: "reasoning",
        id: `rs_${tag}`,
        summary: [{ type: "summary_text", text: `thought ${n}` }],
        encrypted_content: ciphertext(account, n),
      },
    ];
    const tools: Array<{ type?: string }> = body.tools ?? [];
    if (tools.some((t) => t.type === "code_interpreter")) {
      // An automatic container: the one the conversation already used, or a
      // new one.
      if (container === undefined) {
        container = `cntr_${tag}`;
        this.containers.set(container, account);
      }
      output.push({
        type: "code_interpreter_call",
        id: `ci_${tag}`,
        container_id: container,
        code: "print(1 + 1)",
        outputs: [{ type: "logs", logs: "2" }],
        status: "completed",
      });
    }
    if (tools.some((t) => t.type === "function")) {
      output.push({
        type: "function_call",
        id: `fc_${tag}`,
        call_id: `call_${tag}`,
        name: "get_weather",
        arguments: '{"city":"Paris"}',
        status: "completed",
      });
    }
    output.push({
      type: "message",
      id: `msg_${tag}`,
      role: "assistant",
      status: "completed",
      content: [{ type: "output_text", text: `answer ${n}`, annotations: [] }],
    });
    const response = {
      id: `resp_${tag}`,
      object: "response",
      created_at: 1_760_000_000,
      status: "completed",
      model: body.model,
      previous_response_id: body.previous_response_id ?? null,
      output,
      usage: { input_tokens: 3, output_tokens: 2, total_tokens: 5 },
    };
    this.answered.push(response);
    if (!body.stream) return { status: 200, body: response };

    let seq = 0;
    const frame = (event: Record<string, unknown>) =>
      `event: ${event.type}\ndata: ${JSON.stringify({ ...event, sequence_number: seq++ })}\n\n`;
    const pending = { ...response, status: "in_progress", output: [] };
    let sse = frame({ type: "response.created", response: pending });
    output.forEach((item, index) => {
      sse += frame({ type: "response.output_item.added", output_index: index, item: { ...item, status: "in_progress" } });
      if (item.type === "message") {
        sse += frame({
          type: "response.output_text.delta",
          item_id: item.id,
          output_index: index,
          content_index: 0,
          delta: `answer ${n}`,
        });
      }
      if (item.type === "function_call") {
        sse += frame({
          type: "response.function_call_arguments.delta",
          item_id: item.id,
          output_index: index,
          delta: item.arguments,
        });
      }
      if (item.type === "code_interpreter_call") {
        sse += frame({ type: "response.code_interpreter_call.completed", item_id: item.id, output_index: index });
      }
      sse += frame({ type: "response.output_item.done", output_index: index, item });
    });
    sse += frame({ type: "response.completed", response });
    return { status: 200, sse };
  }
}

/** Every JSON `data:` payload of an SSE body, in order. */
function sseEvents(text: string): Array<Record<string, any>> {
  return text.split(/\r?\n\r?\n/).flatMap((frame) => {
    const data = frame
      .split(/\r?\n/)
      .filter((line) => line.startsWith("data:"))
      .map((line) => line.slice(5).replace(/^ /, ""))
      .join("\n");
    if (!data || data === "[DONE]") return [];
    return [JSON.parse(data) as Record<string, any>];
  });
}

/** The Response a buffered answer is, or a stream's `response.completed` carries. */
function finalResponse(answer: { text: string }, stream: boolean): Record<string, any> {
  if (!stream) return JSON.parse(answer.text);
  const completed = sseEvents(answer.text).find((e) => e.type === "response.completed");
  expect(completed, answer.text).toBeDefined();
  return completed!.response;
}

describe("platform mode e2e: Responses conversations that go on", () => {
  let app: SpawnedApp | undefined;
  let accounts: Accounts | undefined;
  let etcdReachable = false;
  /** The ids of the routing group's targets. */
  const targetIds: string[] = [];

  const post = async (key: string, body: unknown): Promise<{ status: number; text: string }> => {
    const res = await fetch(`${app!.proxyUrl}/v1/responses`, {
      method: "POST",
      headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    return { status: res.status, text: await res.text() };
  };

  /** Nothing an answer carries is the upstream's own id or names an account. */
  const expectNoUpstreamIds = (text: string) => {
    expect(text).not.toContain(CHANNEL_MARKER);
    expect(text).not.toContain("acct-");
  };

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    accounts = await Accounts.start();
    app = await spawnApp({
      extraEnv: {
        AISIX_PROXY__PLATFORM__ENABLED: "true",
        AISIX_PROXY__PLATFORM__ID_KEY_ENV: PLATFORM_SECRET_ENV,
        [PLATFORM_SECRET_ENV]: PLATFORM_SECRET,
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);

    const pk = async (account: string) =>
      (
        await seed.createProviderKey({
          display_name: `pk-${account}`,
          provider: "openai",
          adapter: "openai",
          secret: account,
          api_base: accounts!.baseUrl,
        })
      ).id;
    const pkA = await pk("acct-a");
    const pkB = await pk("acct-b");
    const direct = (name: string, pkId: string) =>
      seed.createModel({
        display_name: name,
        provider: "openai",
        model_name: "upstream-model",
        provider_key_id: pkId,
        retries: 0,
      });
    await direct(MODELS.interpreter, pkA);
    const held = await direct(MODELS.held, pkA);
    targetIds.push((await direct(MODELS.groupA, pkA)).id, (await direct(MODELS.groupB, pkB)).id);
    await seed.createModel({
      display_name: MODELS.group,
      routing: {
        strategy: "round_robin",
        targets: [{ model: MODELS.groupA }, { model: MODELS.groupB }],
        max_fallbacks: 1,
      },
    });
    // A block-capable output guardrail holds the whole stream back before
    // releasing it.
    const guardrail = await seed.createGuardrail(
      {
        name: "pc-held-output",
        enabled: true,
        hook_point: "output",
        kind: "keyword",
        patterns: [{ kind: "literal", value: NEVER_SAID }],
      },
      { attach: false },
    );
    await seed.attachGuardrailToModel(guardrail.id, held.id);
    // The caller keys last: each one authenticating implies every resource
    // above is in the gateway's snapshot.
    await seed.createApiKey({ key_hash: hash(KEYS.a), allowed_models: ["*"], team_id: "tenant-a" });
    await seed.createApiKey({ key_hash: hash(KEYS.b), allowed_models: ["*"], team_id: "tenant-b" });
    await waitConfigPropagation(async () => {
      for (const key of Object.values(KEYS)) {
        const res = await new ProxyClient(app!.proxyUrl, key).listModels();
        if (res.status !== 200) return false;
      }
      return true;
    });
  });

  afterAll(async () => {
    // No seal or tag the gateway minted ever reaches an upstream.
    for (const r of accounts?.received ?? []) {
      expect(r.raw).not.toContain("aisix_enc");
    }
    await app?.exit();
    await accounts?.close();
  });

  for (const stream of [false, true]) {
    test(`a code interpreter conversation goes on statelessly, in its own container only (${stream ? "streamed" : "buffered"} first turn)`, async (ctx) => {
      if (!etcdReachable || !app || !accounts) return void ctx.skip();
      const question = { role: "user", content: "compute 1 + 1" };
      const first = await post(KEYS.a, {
        model: MODELS.interpreter,
        input: [question],
        tools: INTERPRETER_TOOLS,
        stream,
      });
      expect(first.status, first.text).toBe(200);
      expectNoUpstreamIds(first.text);
      const upstreamContainer = (accounts.answered.at(-1)!.output as Array<Record<string, any>>).find(
        (i) => i.type === "code_interpreter_call",
      )!.container_id as string;
      expect(upstreamContainer).toMatch(new RegExp(`^cntr_${CHANNEL_MARKER}_acct-a_`));
      const output = finalResponse(first, stream).output as Array<Record<string, any>>;
      const call = output.find((i) => i.type === "code_interpreter_call");
      expect(call, JSON.stringify(output)).toBeDefined();
      const container = call!.container_id as string;
      expect(container).toMatch(/^cntr_/);
      expect(container).not.toBe(upstreamContainer);
      if (stream) {
        // Every frame names the container the same way.
        const named = sseEvents(first.text)
          .flatMap((e) => [e.item, ...((e.response?.output as unknown[]) ?? [])])
          .filter((item: any) => item?.type === "code_interpreter_call")
          .map((item: any) => item.container_id);
        expect(named.length, first.text).toBeGreaterThanOrEqual(3);
        expect(new Set(named), first.text).toEqual(new Set([container]));
      }

      // The next turn sends the whole conversation back.
      const history = [...output];
      const next = (key: string, items: unknown[]) =>
        post(key, {
          model: MODELS.interpreter,
          input: [question, ...items, { role: "user", content: "and 2 + 2?" }],
          tools: INTERPRETER_TOOLS,
        });
      const before = accounts.received.length;
      const second = await next(KEYS.a, history);
      expect(second.status, second.text).toBe(200);
      expectNoUpstreamIds(second.text);
      expect(accounts.received).toHaveLength(before + 1);
      const sent = accounts.last().body.input as Array<Record<string, any>>;
      // The upstream got its own container back, and reused it.
      expect(sent.find((i) => i.type === "code_interpreter_call")?.container_id).toBe(upstreamContainer);
      const again = (JSON.parse(second.text).output as Array<Record<string, any>>).find(
        (i) => i.type === "code_interpreter_call",
      );
      expect(again?.container_id, "the same container comes back as the same id").toBe(container);

      // Another tenant sending the same conversation names a container it
      // was never handed, and the upstream's own id for it is no one's to
      // name: both are refused, naming the field, and no upstream hears of it.
      const at = history.findIndex((i) => i.type === "code_interpreter_call") + 1;
      const refusals = [
        await next(KEYS.b, history),
        await next(
          KEYS.a,
          history.map((i) => (i.type === "code_interpreter_call" ? { ...i, container_id: upstreamContainer } : i)),
        ),
      ];
      for (const refused of refusals) {
        expect(refused.status, refused.text).toBe(400);
        expect(JSON.parse(refused.text).error?.param).toBe(`input[${at}].container_id`);
        expect(refused.text).not.toContain(container);
        expectNoUpstreamIds(refused.text);
      }
      expect(accounts.received).toHaveLength(before + 1);
    });
  }

  test("a stream an output guardrail held back is released with the ids sealed, and they work on the next turn", async (ctx) => {
    if (!etcdReachable || !app || !accounts) return void ctx.skip();
    const question = { role: "user", content: "weather in Paris?" };
    const streamed = await post(KEYS.a, {
      model: MODELS.held,
      input: [question],
      tools: RESPONSES_TOOLS,
      stream: true,
    });
    expect(streamed.status, streamed.text).toBe(200);
    const upstream = accounts.last().raw;
    expect(upstream).toContain(`"stream":true`);
    expectNoUpstreamIds(streamed.text);
    const events = sseEvents(streamed.text);
    // One response id on every snapshot.
    const responseIds = events.filter((e) => typeof e.response?.id === "string").map((e) => e.response.id);
    expect(responseIds.length, streamed.text).toBeGreaterThanOrEqual(2);
    expect(new Set(responseIds).size, responseIds.join(",")).toBe(1);
    expect(responseIds[0]).toMatch(/^resp_/);
    // Each item keeps one sealed id across its frames, its per-item events
    // included, and a call id is shown as a call.
    const done = events.filter((e) => e.type === "response.output_item.done").map((e) => e.item);
    const call = done.find((i) => i.type === "function_call");
    const message = done.find((i) => i.type === "message");
    expect(call?.id).toMatch(/^fc_/);
    expect(call?.call_id).toMatch(/^call_/);
    expect(message?.id).toMatch(/^msg_/);
    for (const e of events.filter((e) => typeof e.item_id === "string")) {
      expect([call.id, message.id], e.type).toContain(e.item_id);
    }
    expect(events.find((e) => e.type === "response.output_item.added" && e.item.type === "function_call")?.item.id).toBe(
      call.id,
    );
    const completed = events.find((e) => e.type === "response.completed")!.response;
    expect(completed.output.find((i: any) => i.type === "function_call").call_id).toBe(call.call_id);

    // A stateless next turn: the upstream gets its own ids back.
    const stateless = await post(KEYS.a, {
      model: MODELS.held,
      input: [question, ...completed.output, { type: "function_call_output", call_id: call.call_id, output: "sunny" }],
      tools: RESPONSES_TOOLS,
    });
    expect(stateless.status, stateless.text).toBe(200);
    expectNoUpstreamIds(stateless.text);
    const echoed = accounts.last().body.input as Array<Record<string, any>>;
    const echoedCall = echoed.find((i) => i.type === "function_call");
    expect(echoedCall?.id).toMatch(new RegExp(`^fc_${CHANNEL_MARKER}_acct-a_`));
    expect(echoedCall?.call_id).toBe(echoedCall?.id.replace(/^fc_/, "call_"));
    expect(echoed.find((i) => i.type === "function_call_output")?.call_id).toBe(echoedCall?.call_id);

    // A stored next turn, from the response id it handed out.
    const stored = await post(KEYS.a, {
      model: MODELS.held,
      previous_response_id: responseIds[0],
      input: [{ type: "function_call_output", call_id: call.call_id, output: "sunny" }],
      tools: RESPONSES_TOOLS,
      stream: true,
    });
    expect(stored.status, stored.text).toBe(200);
    expectNoUpstreamIds(stored.text);
    const continued = accounts.last().body;
    expect(continued.previous_response_id).toMatch(new RegExp(`^resp_${CHANNEL_MARKER}_acct-a_`));
    expect(continued.input[0].call_id).toBe(echoedCall?.call_id);
  });

  test("behind a routing group, encrypted reasoning reaches only the account that wrote it, for its tenant only", async (ctx) => {
    if (!etcdReachable || !app || !accounts) return void ctx.skip();
    const turn = (text: string) => ({ role: "user", content: text });
    let history: unknown[] = [turn("turn 1")];
    const first = await post(KEYS.a, { model: MODELS.group, store: false, input: history });
    expect(first.status, first.text).toBe(200);
    expectNoUpstreamIds(first.text);
    const origin = accounts.last().account;
    const firstBody = JSON.parse(first.text);
    const reasoning = (firstBody.output as Array<Record<string, any>>).find((i) => i.type === "reasoning")!;
    // The ciphertext is tagged with the target that wrote it, sealed to the
    // tenant: the target shows neither by name nor by id.
    expect(reasoning.encrypted_content).toMatch(/^aisix_enc_v2:/);
    for (const target of [MODELS.groupA, MODELS.groupB, ...targetIds]) {
      expect(reasoning.encrypted_content).not.toContain(target);
      expect(
        Buffer.from(reasoning.encrypted_content.slice("aisix_enc_v2:".length).split(";")[0], "base64url").toString(
          "latin1",
        ),
      ).not.toContain(target);
    }
    expect(reasoning.summary[0].text).toMatch(/^thought \d+$/);
    history = [...history, ...firstBody.output];

    // round_robin alternates between the two accounts; every turn that
    // carries the ciphertext must still reach the account that wrote it.
    for (let n = 2; n <= 4; n++) {
      history = [...history, turn(`turn ${n}`)];
      const stream = n % 2 === 0;
      const answer = await post(KEYS.a, { model: MODELS.group, store: false, input: history, stream });
      expect(answer.status, answer.text).toBe(200);
      expectNoUpstreamIds(answer.text);
      const served = accounts.last();
      expect(served.account).toBe(origin);
      const sent = (served.body.input as Array<Record<string, any>>).filter((i) => i.type === "reasoning");
      expect(sent).toHaveLength(n - 1);
      for (const item of sent) expect(item.encrypted_content).toMatch(ciphertextOf(origin));
      history = [...history, ...finalResponse(answer, stream).output];
    }

    // Tenant B holding tenant A's conversation: the tag does not open for
    // it, so no account is handed the ciphertext, and the readable part of
    // the reasoning still goes.
    const foreign = await post(KEYS.b, { model: MODELS.group, store: false, input: history });
    expect(foreign.status, foreign.text).toBe(200);
    const sentForB = accounts.last().body.input as Array<Record<string, any>>;
    const reasoningForB = sentForB.filter((i) => i.type === "reasoning");
    expect(reasoningForB.length).toBeGreaterThan(0);
    for (const item of reasoningForB) {
      expect(item).not.toHaveProperty("encrypted_content");
      expect(item.summary[0].text).toMatch(/^thought \d+$/);
    }
    expect(accounts.last().raw).not.toContain("enc:");

    // A stored continuation through the group lands on the account that
    // stored the response, with its own id.
    const stored = await post(KEYS.a, { model: MODELS.group, input: [turn("remember")] });
    expect(stored.status, stored.text).toBe(200);
    const storedOn = accounts.last().account;
    for (let n = 0; n < 2; n++) {
      const next = await post(KEYS.a, {
        model: MODELS.group,
        previous_response_id: JSON.parse(stored.text).id,
        input: [turn(`and ${n}`)],
      });
      expect(next.status, next.text).toBe(200);
      expect(accounts.last().account).toBe(storedOn);
      expect(accounts.last().body.previous_response_id).toMatch(new RegExp(`^resp_${CHANNEL_MARKER}_${storedOn}_`));
    }
  });
});
