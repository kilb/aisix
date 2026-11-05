import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type OpenAiUpstreamOptions,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: an output `mask` rule hides a value from the caller, whichever field of
// the answer would have shown it.
//
// A caller that asks for token log-probabilities gets the answer a second time,
// one token at a time. A rule that masks an address in the text has to keep it
// out of that copy too, on every surface that relays one, streamed or not. The
// mock upstream splits the address across tokens, as a real model does, so the
// address is in no single token. A choice nothing was masked in keeps what the
// caller asked for.

const KEY = "sk-output-mask-logprobs-e2e";
const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");

const ADDRESS_TOKENS = ["alice", "@", "example.com"];
const ADDRESS = ADDRESS_TOKENS.join("");
const CLEAN_TOKEN = "nothing-to-hide";

const chatLogprobs = (tokens: string[]) => ({
  content: tokens.map((token) => ({
    token,
    logprob: -0.1,
    bytes: [...Buffer.from(token)],
    top_logprobs: [{ token, logprob: -0.1, bytes: [...Buffer.from(token)] }],
  })),
});

const usage = { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 };

const CHAT_BODY = {
  id: "chatcmpl-lp",
  object: "chat.completion",
  created: 1,
  model: "gpt-4o-mini",
  choices: [
    {
      index: 0,
      finish_reason: "stop",
      message: { role: "assistant", content: `write to ${ADDRESS}` },
      logprobs: chatLogprobs(["write to ", ...ADDRESS_TOKENS]),
    },
    {
      index: 1,
      finish_reason: "stop",
      message: { role: "assistant", content: CLEAN_TOKEN },
      logprobs: chatLogprobs([CLEAN_TOKEN]),
    },
  ],
  usage,
};

const chatFrame = (index: number, content: string, tokens: string[]) =>
  JSON.stringify({
    id: "chatcmpl-lp",
    object: "chat.completion.chunk",
    created: 1,
    model: "gpt-4o-mini",
    choices: [
      {
        index,
        delta: { role: "assistant", content },
        logprobs: chatLogprobs(tokens),
        finish_reason: null,
      },
    ],
  });

const CHAT_STREAM = [
  chatFrame(0, "write to ", ["write to "]),
  ...ADDRESS_TOKENS.map((token) => chatFrame(0, token, [token])),
  chatFrame(1, CLEAN_TOKEN, [CLEAN_TOKEN]),
  JSON.stringify({
    id: "chatcmpl-lp",
    object: "chat.completion.chunk",
    created: 1,
    model: "gpt-4o-mini",
    choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
  }),
  "[DONE]",
];

const completionLogprobs = (tokens: string[]) => ({
  tokens,
  token_logprobs: tokens.map(() => -0.1),
  top_logprobs: tokens.map((token) => ({ [token]: -0.1 })),
  text_offset: tokens.map(() => 0),
});

const COMPLETION_BODY = {
  id: "cmpl-lp",
  object: "text_completion",
  created: 1,
  model: "gpt-3.5-turbo-instruct",
  choices: [
    {
      index: 0,
      finish_reason: "stop",
      text: `write to ${ADDRESS}`,
      logprobs: completionLogprobs(["write to ", ...ADDRESS_TOKENS]),
    },
    {
      index: 1,
      finish_reason: "stop",
      text: CLEAN_TOKEN,
      logprobs: completionLogprobs([CLEAN_TOKEN]),
    },
  ],
  usage,
};

const completionFrame = (index: number, text: string) =>
  JSON.stringify({
    id: "cmpl-lp",
    object: "text_completion",
    created: 1,
    model: "gpt-3.5-turbo-instruct",
    choices: [
      { index, text, logprobs: completionLogprobs([text]), finish_reason: null },
    ],
  });

const COMPLETION_STREAM = [
  completionFrame(0, "write to "),
  ...ADDRESS_TOKENS.map((token) => completionFrame(0, token)),
  completionFrame(1, CLEAN_TOKEN),
  "[DONE]",
];

const responsesLogprobs = (tokens: string[]) =>
  tokens.map((token) => ({
    token,
    logprob: -0.1,
    bytes: [...Buffer.from(token)],
    top_logprobs: [],
  }));

const responsesPart = (text: string, tokens: string[]) => ({
  type: "output_text",
  text,
  annotations: [],
  logprobs: responsesLogprobs(tokens),
});

const responseObject = (status: string, output: unknown[]) => ({
  id: "resp-lp",
  object: "response",
  created_at: 1,
  status,
  model: "gpt-4o-mini",
  output,
  usage: { input_tokens: 1, output_tokens: 1, total_tokens: 2 },
});

const responsesMessage = (id: string, part: unknown) => ({
  id,
  type: "message",
  role: "assistant",
  status: "completed",
  content: [part],
});

const MASKED_PART = responsesPart(`write to ${ADDRESS}`, ["write to ", ...ADDRESS_TOKENS]);
const CLEAN_PART = responsesPart(CLEAN_TOKEN, [CLEAN_TOKEN]);

const RESPONSES_BODY = responseObject("completed", [
  responsesMessage("msg-a", MASKED_PART),
  responsesMessage("msg-b", CLEAN_PART),
]);

const responsesDelta = (itemId: string, outputIndex: number, delta: string) =>
  JSON.stringify({
    type: "response.output_text.delta",
    item_id: itemId,
    output_index: outputIndex,
    content_index: 0,
    delta,
    logprobs: responsesLogprobs([delta]),
  });

const RESPONSES_STREAM = [
  JSON.stringify({
    type: "response.created",
    response: responseObject("in_progress", []),
  }),
  responsesDelta("msg-a", 0, "write to "),
  ...ADDRESS_TOKENS.map((token) => responsesDelta("msg-a", 0, token)),
  JSON.stringify({
    type: "response.output_text.done",
    item_id: "msg-a",
    output_index: 0,
    content_index: 0,
    text: `write to ${ADDRESS}`,
    logprobs: MASKED_PART.logprobs,
  }),
  JSON.stringify({
    type: "response.content_part.done",
    item_id: "msg-a",
    output_index: 0,
    content_index: 0,
    part: MASKED_PART,
  }),
  JSON.stringify({
    type: "response.output_item.done",
    output_index: 0,
    item: responsesMessage("msg-a", MASKED_PART),
  }),
  responsesDelta("msg-b", 1, CLEAN_TOKEN),
  JSON.stringify({
    type: "response.completed",
    response: RESPONSES_BODY,
  }),
];

/** The address, a piece of it, or its bytes must appear nowhere in `text`. */
function expectNoAddress(text: string): void {
  expect(text).not.toContain(ADDRESS);
  expect(text).not.toContain("alice");
  expect(text).not.toContain("example.com");
  // The token's `bytes`: `alice` as a JSON byte array.
  expect(text).not.toContain([...Buffer.from("alice")].join(","));
}

describe("output mask keeps a masked value out of token log-probabilities", () => {
  let app: SpawnedApp | undefined;
  let etcdReachable = false;
  const upstreams: OpenAiUpstream[] = [];

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const model = async (name: string, opts: OpenAiUpstreamOptions) => {
      const upstream = await startOpenAiUpstream(opts);
      upstreams.push(upstream);
      const pk = await seed.createProviderKey({
        display_name: `pk-${name}`,
        secret: "sk-mock",
        api_base: `${upstream.baseUrl}/v1`,
      });
      await seed.createModel({
        display_name: name,
        provider: "openai",
        model_name: "gpt-4o-mini",
        provider_key_id: pk.id,
      });
    };
    await model("lp-chat", { nonStreamBody: CHAT_BODY });
    await model("lp-chat-stream", { streamEvents: CHAT_STREAM });
    await model("lp-completion", { nonStreamBody: COMPLETION_BODY });
    await model("lp-completion-stream", { streamEvents: COMPLETION_STREAM });
    await model("lp-responses", { nonStreamBody: RESPONSES_BODY });
    await model("lp-responses-stream", { streamEvents: RESPONSES_STREAM });
    await seed.createApiKey({ key_hash: sha256(KEY), allowed_models: ["*"] });
    await seed.createGuardrail({
      name: "lp-mask",
      enabled: true,
      hook_point: "output",
      kind: "pii",
      detectors: [{ type: "email", action: "mask" }],
    });

    const chat = () =>
      call("/v1/chat/completions", {
        model: "lp-chat",
        messages: [{ role: "user", content: "hi" }],
      });
    // The rule is in force once the address stops coming back.
    await waitConfigPropagation(async () => {
      try {
        const r = await chat();
        return r.status === 200 && !(await r.text()).includes(ADDRESS);
      } catch {
        return false;
      }
    });
  });

  afterAll(async () => {
    await app?.exit();
    await Promise.all(upstreams.map((u) => u.close()));
  });

  const call = (path: string, body: unknown) =>
    fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${KEY}`,
        "content-type": "application/json",
      },
      body: JSON.stringify(body),
    });

  test("/v1/chat/completions", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const r = await call("/v1/chat/completions", {
      model: "lp-chat",
      messages: [{ role: "user", content: "hi" }],
      logprobs: true,
      top_logprobs: 1,
      n: 2,
    });
    expect(r.status).toBe(200);
    const text = await r.text();
    expectNoAddress(text);
    const body = JSON.parse(text) as {
      choices: { message: { content: string }; logprobs?: unknown }[];
    };
    expect(body.choices[0].message.content).toContain("write to ");
    expect(body.choices[1].logprobs).toEqual(chatLogprobs([CLEAN_TOKEN]));
  });

  test("/v1/chat/completions streamed", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const r = await call("/v1/chat/completions", {
      model: "lp-chat-stream",
      messages: [{ role: "user", content: "hi" }],
      logprobs: true,
      n: 2,
      stream: true,
    });
    expect(r.status).toBe(200);
    const text = await r.text();
    expectNoAddress(text);
    expect(text).toContain("write to ");
    expect(text).toContain(`"token":"${CLEAN_TOKEN}"`);
  });

  test("/v1/completions", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const r = await call("/v1/completions", {
      model: "lp-completion",
      prompt: "hi",
      logprobs: 1,
      n: 2,
    });
    expect(r.status).toBe(200);
    const text = await r.text();
    expectNoAddress(text);
    const body = JSON.parse(text) as {
      choices: { text: string; logprobs?: { tokens: string[] } }[];
    };
    expect(body.choices[0].text).toContain("write to ");
    expect(body.choices[1].logprobs?.tokens).toEqual([CLEAN_TOKEN]);
  });

  test("/v1/completions streamed", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const r = await call("/v1/completions", {
      model: "lp-completion-stream",
      prompt: "hi",
      logprobs: 1,
      n: 2,
      stream: true,
    });
    expect(r.status).toBe(200);
    const text = await r.text();
    expectNoAddress(text);
    expect(text).toContain("write to ");
    expect(text).toContain(`"tokens":["${CLEAN_TOKEN}"]`);
  });

  test("/v1/responses", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const r = await call("/v1/responses", {
      model: "lp-responses",
      input: "hi",
      include: ["message.output_text.logprobs"],
    });
    expect(r.status).toBe(200);
    const text = await r.text();
    expectNoAddress(text);
    expect(text).toContain("write to ");
    expect(text).toContain(`"token":"${CLEAN_TOKEN}"`);
  });

  test("/v1/responses streamed", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const r = await call("/v1/responses", {
      model: "lp-responses-stream",
      input: "hi",
      include: ["message.output_text.logprobs"],
      stream: true,
    });
    expect(r.status).toBe(200);
    const text = await r.text();
    expectNoAddress(text);
    expect(text).toContain("write to ");
    expect(text).toContain(`"token":"${CLEAN_TOKEN}"`);
  });
});
