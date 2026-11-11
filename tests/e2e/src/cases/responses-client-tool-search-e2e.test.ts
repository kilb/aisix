import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  pickFreePort,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";

// 源盲契约：client search → 客户端发现工具 → 调用新工具 → 回传结果 → 最终文本。
// https://developers.openai.com/api/docs/guides/tools-tool-search#client-executed-tool-search
// https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response_tool_search_call.py
// https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response_tool_search_output_item_param.py
// https://github.com/openai/openai-python/blob/main/src/openai/types/responses/tool_search_output_namespace_tool.py
// https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response_custom_tool_call.py
// Chat 供应商只实现标准 function tool wire，不实现 Responses 或托管 search：
// https://platform.openai.com/docs/api-reference/chat/create

type Json = Record<string, any>;

const CALLER_KEY = "sk-e2e-client-tool-search";
const PLATFORM_ENV = "E2E_CLIENT_TOOL_SEARCH_ID_SECRET";
const PLATFORM_SECRET = "e0e5d4c3b2a190876543210fedcba9876543210abcdef0123456789abcdef012";
const PRIVATE_MARKER = "provider_private";
const SEARCH_CALL_ID = `call_${PRIVATE_MARKER}_search`;
const SECOND_SEARCH_CALL_ID = `call_${PRIVATE_MARKER}_search_again`;
const LOOKUP_CALL_ID = `call_${PRIVATE_MARKER}_lookup`;
const TIMEZONE_CALL_ID = `call_${PRIVATE_MARKER}_timezone`;
const PATCH_CALL_ID = `call_${PRIVATE_MARKER}_patch`;
const FINAL_TEXT = "09:00 in Europe/Paris; patch applied.";
const PATCH = "*** Begin Patch\n*** Update File: meeting.txt\n@@\n-old\n+09:00\n*** End Patch";
const SEARCH_ARGUMENTS = { query: "calendar lookup and workspace patch", limit: 3 };
const LOOKUP_ARGUMENTS = { date: "2026-11-11", attendees: ["alice", "bob"] };
const TIMEZONE_ARGUMENTS = { city: "Paris" };
const PATCH_ARGUMENTS = { content: PATCH };

const SEARCH = {
  type: "tool_search",
  execution: "client",
  description: "Discover project tools for the requested task.",
  parameters: {
    type: "object",
    properties: {
      query: { type: "string", description: "Tools needed to continue" },
      limit: { type: "integer", minimum: 1, maximum: 10 },
    },
    required: ["query", "limit"],
    additionalProperties: false,
  },
};

// 普通函数恰好同名：发现工具不能覆盖它，也不能把它误标为 client search。
const ORIGINAL_TOOL = {
  type: "function",
  name: "tool_search",
  description: "Read a local tool category without discovering remote tools.",
  parameters: {
    type: "object",
    properties: { category: { type: "string", enum: ["local"] } },
    required: ["category"],
    additionalProperties: false,
  },
  strict: true,
};
const LOOKUP = {
  type: "function",
  name: "lookup",
  description: "Look up calendar meetings for a date and attendees.",
  defer_loading: true,
  parameters: {
    type: "object",
    properties: {
      date: { type: "string", description: "ISO date" },
      attendees: { type: "array", items: { type: "string" } },
    },
    required: ["date", "attendees"],
    additionalProperties: false,
  },
  strict: true,
};
const TIMEZONE = {
  type: "function",
  name: "get_timezone",
  description: "Find the IANA timezone for a city.",
  defer_loading: true,
  parameters: {
    type: "object",
    properties: { city: { type: "string" } },
    required: ["city"],
    additionalProperties: false,
  },
  strict: true,
};
const CUSTOM_TOOL = {
  type: "custom",
  name: "apply_patch",
  description: "Apply a workspace patch.",
  format: { type: "text" },
  defer_loading: true,
};
const LOADED_TOOLS = [
  TIMEZONE,
  { type: "namespace", name: "calendar", description: "Calendar tools.", tools: [LOOKUP] },
  { type: "namespace", name: "workspace", description: "Workspace tools.", tools: [CUSTOM_TOOL] },
];
const CLIENT_RESULTS = [
  { meetings: [{ time: "09:00", attendees: ["alice", "bob"] }] },
  { timezone: "Europe/Paris" },
  "Patch applied successfully",
];

interface Received {
  path: string;
  body: Json;
  calls: Json[];
}

// 根据供应商收到的公开 schema 选择函数；不依赖网关给桥接函数取的名字。
function offered(body: Json, property: string): Json | undefined {
  return (body.tools ?? []).find((tool: Json) =>
    Object.hasOwn(tool.function?.parameters?.properties ?? {}, property),
  );
}

async function startChatProvider(): Promise<{
  baseUrl: string;
  received: Received[];
  close(): Promise<void>;
}> {
  const received: Received[] = [];
  const server: Server = createServer((req, res) => {
    let raw = "";
    req.on("data", (chunk: Buffer) => { raw += chunk.toString("utf8"); });
    req.on("end", () => {
      const body = JSON.parse(raw) as Json;
      const toolsReturned = (body.messages ?? []).filter((message: Json) => message.role === "tool");
      const call = (id: string, tool: Json, args: Json) => ({
        id, type: "function", function: { name: tool.function.name, arguments: JSON.stringify(args) },
      });
      let calls: Json[] = [];
      let content: string | null = null;
      const searchTwice = (body.messages ?? []).some((message: Json) => message.content === "Discover calendar tools in two searches.");
      if (toolsReturned.some((message: Json) => message.tool_call_id === PATCH_CALL_ID)) {
        content = FINAL_TEXT;
      } else if (searchTwice && toolsReturned.some((message: Json) => message.tool_call_id === SECOND_SEARCH_CALL_ID)) {
        content = "Calendar tools are available.";
      } else if (searchTwice && toolsReturned.some((message: Json) => message.tool_call_id === SEARCH_CALL_ID)) {
        const search = offered(body, "query");
        if (search) calls = [call(SECOND_SEARCH_CALL_ID, search, SEARCH_ARGUMENTS)];
        else content = "No callable search tool was provided.";
      } else if (toolsReturned.some((message: Json) => message.tool_call_id === SEARCH_CALL_ID)) {
        const lookup = offered(body, "date");
        const timezone = offered(body, "city");
        const patch = offered(body, "content");
        if (lookup && timezone && patch) {
          calls = [
            call(LOOKUP_CALL_ID, lookup, LOOKUP_ARGUMENTS),
            call(TIMEZONE_CALL_ID, timezone, TIMEZONE_ARGUMENTS),
            call(PATCH_CALL_ID, patch, PATCH_ARGUMENTS),
          ];
        } else content = "The discovered tools were unavailable.";
      } else {
        const search = offered(body, "query");
        const original = offered(body, "category");
        if (search) calls = [call(SEARCH_CALL_ID, search, SEARCH_ARGUMENTS)];
        else if (original) calls = [call(`call_${PRIVATE_MARKER}_ordinary`, original, { category: "local" })];
        else content = "No callable search tool was provided.";
      }
      received.push({ path: req.url ?? "", body, calls });
      if (req.url !== "/v1/chat/completions") {
        res.writeHead(404, { "content-type": "application/json" });
        res.end(JSON.stringify({ error: { type: "invalid_request_error", message: "Only Chat Completions is available." } }));
        return;
      }
      const usage = { prompt_tokens: 40, completion_tokens: 12, total_tokens: 52 };
      const finishReason = calls.length ? "tool_calls" : "stop";
      if (!body.stream) {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({
          id: `chatcmpl_${PRIVATE_MARKER}`, object: "chat.completion", created: 1_700_000_000,
          model: body.model,
          choices: [{ index: 0, message: { role: "assistant", content, ...(calls.length ? { tool_calls: calls } : {}) }, finish_reason: finishReason }],
          usage,
        }));
        return;
      }
      res.writeHead(200, { "content-type": "text/event-stream" });
      const chunk = (delta: Json, finish: string | null = null, tokenUsage?: Json) =>
        res.write(`data: ${JSON.stringify({
          id: `chatcmpl_${PRIVATE_MARKER}`, object: "chat.completion.chunk", created: 1_700_000_000,
          model: body.model, choices: [{ index: 0, delta, finish_reason: finish }],
          ...(tokenUsage ? { usage: tokenUsage } : {}),
        })}\n\n`);
      chunk({ role: "assistant", content: "" });
      calls.forEach((toolCall, index) => {
        chunk({ tool_calls: [{ index, ...toolCall, function: { name: toolCall.function.name, arguments: "" } }] });
        const args = toolCall.function.arguments as string;
        const split = Math.floor(args.length / 2);
        chunk({ tool_calls: [{ index, function: { arguments: args.slice(0, split) } }] });
        chunk({ tool_calls: [{ index, function: { arguments: args.slice(split) } }] });
      });
      if (content !== null) chunk({ content });
      chunk({}, finishReason, usage);
      res.end("data: [DONE]\n\n");
    });
  });
  const port = await pickFreePort();
  await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", resolve));
  return {
    baseUrl: `http://127.0.0.1:${port}/v1`, received,
    close: () => new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve())),
  };
}

function events(text: string): Json[] {
  return text.split(/\r?\n\r?\n/).flatMap((frame) => {
    const data = frame.split(/\r?\n/).filter((line) => line.startsWith("data:"))
      .map((line) => line.slice(5).trimStart()).join("\n");
    return data && data !== "[DONE]" ? [JSON.parse(data) as Json] : [];
  });
}

function completed(text: string, stream: boolean): Json {
  if (!stream) return JSON.parse(text) as Json;
  const frames = events(text);
  expect(frames.filter((event) => event.type === "response.completed"), text).toHaveLength(1);
  expect(frames.some((event) => event.type === "response.failed"), text).toBe(false);
  return frames.find((event) => event.type === "response.completed")!.response;
}

function expectPlatformCall(item: Json, upstreamId: string): void {
  expect(typeof item.id).toBe("string");
  expect(item.id.length).toBeGreaterThan(0);
  expect(typeof item.call_id).toBe("string");
  expect(item.call_id.length).toBeGreaterThan(0);
  expect(item.call_id).not.toBe(upstreamId);
}

function expectStreamItem(text: string, item: Json): void {
  const frames = events(text);
  const added = frames.filter((event) => event.type === "response.output_item.added" && event.item.id === item.id);
  const done = frames.filter((event) => event.type === "response.output_item.done" && event.item.id === item.id);
  expect(added, text).toHaveLength(1);
  expect(done, text).toHaveLength(1);
  expect(added[0].item).toMatchObject({ type: item.type, call_id: item.call_id });
  expect(done[0].item).toEqual(item);
  expect(frames.indexOf(added[0])).toBeLessThan(frames.indexOf(done[0]));
  expect(frames.indexOf(done[0])).toBeLessThan(frames.findIndex((event) => event.type === "response.completed"));
}

function expectOriginal(body: Json): void {
  expect((body.tools ?? []).filter((tool: Json) => tool.function?.name === ORIGINAL_TOOL.name)).toEqual([
    { type: "function", function: {
      name: "tool_search", description: ORIGINAL_TOOL.description,
      parameters: ORIGINAL_TOOL.parameters, strict: true,
    } },
  ]);
}

function expectReplay(body: Json, calls: Json[], results: unknown[], argumentFormat: "raw" | "search-json" = "raw"): void {
  const assistantCalls = (body.messages ?? []).flatMap((message: Json) => message.tool_calls ?? []);
  const toolResults = (body.messages ?? []).filter((message: Json) => message.role === "tool");
  calls.forEach((call, index) => {
    const replayedCalls = assistantCalls.filter((entry: Json) => entry.id === call.id);
    if (argumentFormat === "search-json") {
      expect(replayedCalls).toEqual([{
        ...call, function: { ...call.function, arguments: expect.any(String) },
      }]);
      expect(JSON.parse(replayedCalls[0].function.arguments)).toEqual(JSON.parse(call.function.arguments));
    } else {
      expect(replayedCalls).toEqual([call]);
    }
    const replay = toolResults.filter((entry: Json) => entry.tool_call_id === call.id);
    expect(replay).toHaveLength(1);
    const expected = typeof results[index] === "string" ? results[index] : JSON.stringify(results[index]);
    if (argumentFormat === "search-json") {
      expect(typeof replay[0].content).toBe("string");
      expect(JSON.parse(replay[0].content)).toEqual(results[index]);
    } else {
      expect(replay[0].content).toBe(expected);
    }
  });
}

describe("platform Responses client tool search through a Chat-only custom provider", () => {
  let app: SpawnedApp | undefined;
  let upstream: Awaited<ReturnType<typeof startChatProvider>> | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    upstream = await startChatProvider();
    app = await spawnApp({ extraEnv: {
      AISIX_PROXY__PLATFORM__ENABLED: "true",
      AISIX_PROXY__PLATFORM__ID_KEY_ENV: PLATFORM_ENV,
      [PLATFORM_ENV]: PLATFORM_SECRET,
    } });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    for (const provider of ["rpm", "matrix"]) {
      // 自定义 provider + openai adapter，没有 apis，重现生产环境的 Chat 桥接选择。
      const pk = await seed.createProviderKey({
        display_name: `client-search-${provider}`, provider, adapter: "openai",
        secret: "sk-test-upstream", api_base: upstream.baseUrl,
      });
      await seed.createModel({
        display_name: `client-search-${provider}`, provider, model_name: "chat-only-model",
        provider_key_id: pk.id, retries: 0,
      });
    }
    // 最后写 caller key；认证成功意味着此前全部资源已应用。
    await seed.createApiKey({
      key_hash: createHash("sha256").update(CALLER_KEY).digest("hex"),
      allowed_models: ["*"], team_id: "client-search-tenant",
    });
    await waitConfigPropagation(async () =>
      (await new ProxyClient(app!.proxyUrl, CALLER_KEY).listModels()).status === 200,
    );
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  async function post(provider: string, stream: boolean, input: unknown[], tools: unknown[] = [ORIGINAL_TOOL, SEARCH]): Promise<{ text: string; response: Json; sent: Received }> {
    const before = upstream!.received.length;
    const res = await fetch(`${app!.proxyUrl}/v1/responses`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER_KEY}`, "content-type": "application/json" },
      body: JSON.stringify({ model: `client-search-${provider}`, input, tools, tool_choice: "auto", parallel_tool_calls: true, stream }),
    });
    const text = await res.text();
    expect(res.status, text).toBe(200);
    expect(upstream!.received).toHaveLength(before + 1);
    const sent = upstream!.received[before];
    expect(sent.path).toBe("/v1/chat/completions");
    expect(sent.body.stream === true).toBe(stream);
    expect(text).not.toContain(PRIVATE_MARKER);
    return { text, response: completed(text, stream), sent };
  }

  for (const provider of ["rpm", "matrix"]) {
    for (const stream of [false, true]) {
      test(`${provider}: discover, call function/custom tools and finish (${stream ? "stream" : "buffered"})`, async (ctx) => {
        if (!etcdReachable || !app || !upstream) return void ctx.skip();
        const question = { role: "user", content: "Find my meeting, its timezone, and patch meeting.txt." };
        const first = await post(provider, stream, [question]);
        expectOriginal(first.sent.body);
        expect(first.sent.body.tools).toHaveLength(2);
        const offeredSearch = offered(first.sent.body, "query");
        expect(offeredSearch).toBeDefined();
        expect(offeredSearch!.function).toMatchObject({ description: SEARCH.description, parameters: SEARCH.parameters });
        expect(offeredSearch!.function.name).not.toBe(ORIGINAL_TOOL.name);
        expect(first.sent.body.tool_choice).toBe("auto");
        expect(first.sent.body.parallel_tool_calls).toBe(true);
        expect(first.response.output).toHaveLength(1);
        const search = first.response.output[0];
        expect(search).toMatchObject({ type: "tool_search_call", execution: "client", status: "completed", arguments: SEARCH_ARGUMENTS });
        expect(search).not.toHaveProperty("name");
        expectPlatformCall(search, SEARCH_CALL_ID);
        if (stream) {
          expectStreamItem(first.text, search);
          expect(events(first.text).filter((event) => event.type.startsWith("response.function_call_arguments."))).toHaveLength(0);
        }

        const searchOutput = { type: "tool_search_output", execution: "client", call_id: search.call_id, status: "completed", tools: LOADED_TOOLS };
        const history = [question, ...first.response.output, searchOutput];
        const second = await post(provider, stream, history);
        expectOriginal(second.sent.body);
        expect(second.sent.body.tools).toHaveLength(5);
        for (const [property, definition] of [["date", LOOKUP], ["city", TIMEZONE]] as const) {
          const tool = offered(second.sent.body, property);
          expect(tool).toBeDefined();
          expect(tool!.function).toMatchObject({ parameters: definition.parameters, strict: true });
        }
        const patchTool = offered(second.sent.body, "content");
        expect(patchTool).toBeDefined();
        expect(patchTool!.function.parameters).toMatchObject({
          type: "object", properties: { content: { type: "string" } }, required: ["content"],
        });
        expect(Object.keys(patchTool!.function.parameters.properties)).toEqual(["content"]);
        expectReplay(second.sent.body, first.sent.calls, [LOADED_TOOLS], "search-json");
        expect(second.response.output).toHaveLength(3);
        const [lookup, timezone, patch] = second.response.output as Json[];
        expect(lookup).toMatchObject({ type: "function_call", name: "lookup", namespace: "calendar", status: "completed", arguments: JSON.stringify(LOOKUP_ARGUMENTS) });
        expect(timezone).toMatchObject({ type: "function_call", name: "get_timezone", status: "completed", arguments: JSON.stringify(TIMEZONE_ARGUMENTS) });
        expect(patch).toMatchObject({ type: "custom_tool_call", name: "apply_patch", namespace: "workspace", input: PATCH });
        expectPlatformCall(lookup, LOOKUP_CALL_ID);
        expectPlatformCall(timezone, TIMEZONE_CALL_ID);
        expectPlatformCall(patch, PATCH_CALL_ID);
        expect(new Set([lookup.call_id, timezone.call_id, patch.call_id]).size).toBe(3);
        if (stream) {
          for (const item of [lookup, timezone, patch]) expectStreamItem(second.text, item);
          const frames = events(second.text);
          for (const item of [lookup, timezone]) {
            const argumentDeltas = frames.filter((event) => event.type === "response.function_call_arguments.delta" && event.item_id === item.id);
            expect(argumentDeltas.length).toBeGreaterThan(0);
            expect(argumentDeltas.map((event) => event.delta).join("")).toBe(item.arguments);
          }
          const patchDeltas = frames.filter((event) => event.type === "response.custom_tool_call_input.delta" && event.item_id === patch.id);
          expect(patchDeltas.length).toBeGreaterThan(0);
          expect(patchDeltas.map((event) => event.delta).join("")).toBe(PATCH);
        }

        const third = await post(provider, stream, [
          ...history, ...second.response.output,
          { type: "function_call_output", call_id: lookup.call_id, output: JSON.stringify(CLIENT_RESULTS[0]) },
          { type: "function_call_output", call_id: timezone.call_id, output: JSON.stringify(CLIENT_RESULTS[1]) },
          { type: "custom_tool_call_output", call_id: patch.call_id, output: CLIENT_RESULTS[2] },
        ]);
        expectOriginal(third.sent.body);
        expect(third.sent.body.tools).toEqual(second.sent.body.tools);
        expectReplay(third.sent.body, first.sent.calls, [LOADED_TOOLS], "search-json");
        expectReplay(third.sent.body, second.sent.calls, CLIENT_RESULTS);
        expect(third.response.status).toBe("completed");
        expect(third.response.output).toHaveLength(1);
        expect(third.response.output[0]).toMatchObject({ type: "message", role: "assistant", status: "completed" });
        expect(third.response.output[0].content).toEqual([{ type: "output_text", text: FINAL_TEXT, annotations: [] }]);
        if (stream) {
          expect(events(third.text).filter((event) => event.type === "response.output_text.delta").map((event) => event.delta).join("")).toBe(FINAL_TEXT);
        }
      });
    }
  }

  test("repeated discoveries merge namespace members once and stay within this conversation", async (ctx) => {
    if (!etcdReachable || !app || !upstream) return void ctx.skip();
    const question = { role: "user", content: "Discover calendar tools in two searches." };
    const first = await post("rpm", false, [question]);
    expect(first.response.output).toHaveLength(1);
    expect(first.response.output[0].type).toBe("tool_search_call");
    const firstTools = [{ type: "namespace", name: "calendar", description: "Calendar tools.", tools: [LOOKUP] }];
    const firstHistory = [question, ...first.response.output, {
      type: "tool_search_output", execution: "client", call_id: first.response.output[0].call_id,
      status: "completed", tools: firstTools,
    }];
    const second = await post("rpm", false, firstHistory);
    expect(second.response.output).toHaveLength(1);
    expect(second.response.output[0]).toMatchObject({ type: "tool_search_call", execution: "client", arguments: SEARCH_ARGUMENTS });
    const secondTools = [{ type: "namespace", name: "calendar", description: "Calendar tools.", tools: [TIMEZONE, LOOKUP] }];
    const third = await post("rpm", false, [...firstHistory, ...second.response.output, {
      type: "tool_search_output", execution: "client", call_id: second.response.output[0].call_id,
      status: "completed", tools: secondTools,
    }]);
    expectOriginal(third.sent.body);
    expect(third.sent.body.tools).toHaveLength(4);
    const names = third.sent.body.tools.map((tool: Json) => tool.function.name);
    expect(new Set(names).size).toBe(names.length);
    expect(offered(third.sent.body, "date")?.function.parameters).toEqual(LOOKUP.parameters);
    expect(offered(third.sent.body, "city")?.function.parameters).toEqual(TIMEZONE.parameters);
    expectReplay(third.sent.body, first.sent.calls, [firstTools], "search-json");
    expectReplay(third.sent.body, second.sent.calls, [secondTools], "search-json");

    // 新请求没有回放发现结果，不能获得前一会话的动态能力。
    const independent = await post("rpm", false, [question]);
    expectOriginal(independent.sent.body);
    expect(independent.sent.body.tools).toHaveLength(2);
    expect(offered(independent.sent.body, "date")).toBeUndefined();
    expect(offered(independent.sent.body, "city")).toBeUndefined();
    expect(offered(independent.sent.body, "content")).toBeUndefined();
    expect(independent.response.output[0]).toMatchObject({ type: "tool_search_call", execution: "client" });
  });

  for (const execution of ["server", undefined]) {
    test(`hosted search (${execution ?? "default"}) does not impersonate a client-executed tool`, async (ctx) => {
      if (!etcdReachable || !app || !upstream) return void ctx.skip();
      const result = await post("rpm", false, [{ role: "user", content: "Read the local category." }], [
        ORIGINAL_TOOL, { type: "tool_search", ...(execution ? { execution } : {}) },
      ]);
      expectOriginal(result.sent.body);
      expect(result.sent.body.tools).toHaveLength(1);
      expect(result.response.output).toHaveLength(1);
      expect(result.response.output[0]).toMatchObject({ type: "function_call", name: "tool_search", arguments: '{"category":"local"}' });
      expect(result.response.output[0]).not.toHaveProperty("execution");
    });
  }
});
