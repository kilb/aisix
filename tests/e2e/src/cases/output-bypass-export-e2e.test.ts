import { createHash, randomUUID } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  startA2aUpstream,
  startMcpUpstream,
  startMockSls,
  startOpenAiUpstream,
  waitConfigPropagation,
  waitForSlsLog,
  type A2aUpstream,
  type McpUpstream,
  type MockSls,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// 在 `content_mode: full` 下，行里导出的回答必须是护栏读过的回答。输出检查出错
// 而按配置放行（fail open）时，调用方照样拿到回答，但没有任何护栏读过它：
// 这一行要记下这次放行，且不带回答原文。对照：同样的调用，输出检查正常跑完，
// 行里照常有回答 —— 说明缺的只是没人读过的那一段，而不是导出本身坏了。
//
// 覆盖 MCP 工具调用、A2A（整段与流式）、透传路由（整段与流式）。

const OPEN_KEY = "sk-output-bypass-open";
const READ_KEY = "sk-output-bypass-read";
const CREDENTIAL_REF = "mock";
const LOGSTORE = "output-bypass";
const MCP = "bypassmcp";
const AGENT = "bypass-agent";
const ROUTE = "/bypass-route";
const STREAM_ROUTE = "/bypass-stream-route";

const QUESTION = "unread-tool-question-62ad";
// `reverse` 工具把参数倒过来作答，回答里的这串字只会出现在回答里。
const TOOL_ANSWER = [...QUESTION].reverse().join("");
const AGENT_ANSWER = "unread-agent-answer-0c4e";
const AGENT_STREAM_ANSWER = "unread-agent-stream-answer-91f7";
const ROUTE_ANSWER = "unread-route-answer-d38b";
const ROUTE_STREAM_ANSWER = "unread-route-stream-answer-7a15";

const checkOutput = (body: string) => `export function checkOutput() { ${body} }`;

const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");

describe("an answer no guardrail read stays out of a full-content export", () => {
  let etcdReachable = false;
  let sls: MockSls | undefined;
  let upstream: OpenAiUpstream | undefined;
  let streamUpstream: OpenAiUpstream | undefined;
  let agent: A2aUpstream | undefined;
  let mcp: McpUpstream | undefined;
  let app: SpawnedApp | undefined;

  const post = async (key: string, path: string, body: unknown) => {
    const res = await fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${key}`,
        "content-type": "application/json",
        accept: "application/json, text/event-stream",
      },
      body: JSON.stringify(body),
    });
    return { status: res.status, text: await res.text(), requestId: res.headers.get("x-aisix-request-id") };
  };

  const rpc = (method: string, params: unknown) => ({ jsonrpc: "2.0", id: 1, method, params });
  const agentMessage = { message: { role: "user", parts: [{ kind: "text", text: "hi" }], messageId: "m-1" } };
  const chat = (stream: boolean) => ({ model: "m", stream, messages: [{ role: "user", content: "hi" }] });

  const SURFACES: Array<[string, (key: string) => ReturnType<typeof post>, string]> = [
    [
      "mcp tool call",
      (key) => post(key, "/mcp", rpc("tools/call", { name: `${MCP}__reverse`, arguments: { text: QUESTION } })),
      TOOL_ANSWER,
    ],
    ["a2a message/send", (key) => post(key, `/a2a/${AGENT}`, rpc("message/send", agentMessage)), AGENT_ANSWER],
    [
      "a2a message/stream",
      (key) => post(key, `/a2a/${AGENT}`, rpc("message/stream", agentMessage)),
      AGENT_STREAM_ANSWER,
    ],
    ["passthrough", (key) => post(key, `${ROUTE}/v1/chat/completions`, chat(false)), ROUTE_ANSWER],
    [
      "passthrough stream",
      (key) => post(key, `${STREAM_ROUTE}/v1/chat/completions`, chat(true)),
      ROUTE_STREAM_ANSWER,
    ],
  ];

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    sls = await startMockSls();
    upstream = await startOpenAiUpstream({
      nonStreamBody: {
        id: "c",
        object: "chat.completion",
        choices: [{ index: 0, message: { role: "assistant", content: ROUTE_ANSWER }, finish_reason: "stop" }],
      },
    });
    // 流式回答走另一个上游：这个模拟上游只要配了事件就总是流式作答。
    streamUpstream = await startOpenAiUpstream({
      streamEvents: [
        JSON.stringify({
          id: "c",
          object: "chat.completion.chunk",
          choices: [{ index: 0, delta: { content: ROUTE_STREAM_ANSWER }, finish_reason: null }],
        }),
        JSON.stringify({ id: "c", object: "chat.completion.chunk", choices: [{ index: 0, delta: {}, finish_reason: "stop" }] }),
        "[DONE]",
      ],
    });
    agent = await startA2aUpstream({ unaryAnswer: AGENT_ANSWER, streamMessage: AGENT_STREAM_ANSWER });
    mcp = await startMcpUpstream(MCP);
    app = await spawnApp({
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-akid",
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: "output-bypass",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: "aisix-e2e-obs",
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "full",
    });
    const routePk = await seed.createProviderKey({
      display_name: "bypass-route-pk",
      secret: "sk-mock",
      api_base: upstream.baseUrl,
    });
    for (const [name, prefix, target] of [
      ["bypass-route", ROUTE, upstream.baseUrl],
      ["bypass-stream-route", STREAM_ROUTE, streamUpstream.baseUrl],
    ] as const) {
      await seed.createPassthroughRoute({
        name,
        path_prefix: prefix,
        target_url: target,
        provider_key_id: routePk.id,
      });
    }
    await seed.update("a2a_agents", randomUUID(), {
      name: AGENT,
      url: agent.url,
      protocol_version: "0.3",
      auth_type: "none",
      enabled: true,
    });
    await seed.update("mcp_servers", randomUUID(), { display_name: MCP, url: mcp.url, enabled: true });

    // 每把调用方的 key 各挂一条输出检查：一条脚本出错、按配置放行；一条正常跑完。
    const openKeyId = randomUUID();
    const readKeyId = randomUUID();
    for (const [keyId, script, failOpen] of [
      [openKeyId, checkOutput("throw new Error('screening service down');"), true],
      [readKeyId, checkOutput("return { action: 'none' };"), false],
    ] as const) {
      const guardrail = await seed.createGuardrail(
        {
          name: `output-check-${failOpen ? "open" : "read"}`,
          enabled: true,
          hook_point: "output",
          kind: "custom",
          timeout_ms: 5000,
          output_fail_open: failOpen,
          script,
        },
        { attach: false },
      );
      await seed.update("guardrail_attachments", randomUUID(), {
        guardrail_id: guardrail.id,
        scope_type: "api_key",
        scope_id: keyId,
        priority: 100,
      });
    }

    const access = {
      allowed_models: ["*"],
      allowed_mcp_servers: ["*"],
      mcp_access: { allow: ["*"] },
      allowed_agents: ["*"],
      allowed_routes: ["*"],
    };
    await seed.update("api_keys", openKeyId, { key_hash: sha256(OPEN_KEY), ...access });
    await seed.update("api_keys", readKeyId, { key_hash: sha256(READ_KEY), ...access });
    await waitConfigPropagation(async () => {
      for (const key of [OPEN_KEY, READ_KEY]) {
        const res = await fetch(`${app!.proxyUrl}/v1/models`, { headers: { authorization: `Bearer ${key}` } });
        await res.arrayBuffer();
        if (res.status !== 200) return false;
      }
      return true;
    });
  }, 120_000);

  afterAll(async () => {
    await app?.exit();
    await sls?.close();
    await upstream?.close();
    await streamUpstream?.close();
    await agent?.close();
    await mcp?.close();
  });

  const rowOf = async (label: string, requestId: string | null) => {
    expect(requestId, `${label}: the response names its request`).toBeTruthy();
    return waitForSlsLog(sls!, LOGSTORE, (r) => r.get("request_id") === requestId, `${label}: the row`);
  };

  test.for(SURFACES)(
    "%s: an answer a check failed open on is delivered but not exported",
    { timeout: 30_000 },
    async ([label, call, answer], ctx) => {
      if (!etcdReachable) return ctx.skip();
      const res = await call(OPEN_KEY);
      expect(res.status, `${label}: ${res.text.slice(0, 300)}`).toBe(200);
      expect(res.text, `${label}: the caller gets the answer`).toContain(answer);
      const row = await rowOf(label, res.requestId);
      expect(row.get("guardrail_bypassed_reason") ?? "", `${label}: the row records the bypass`).not.toBe("");
      expect([...row.values()].join("\n"), `${label}: the row exports an answer nothing read`).not.toContain(answer);
    },
  );

  test.for(SURFACES)(
    "%s: an answer the check read is exported",
    { timeout: 30_000 },
    async ([label, call, answer], ctx) => {
      if (!etcdReachable) return ctx.skip();
      const res = await call(READ_KEY);
      expect(res.status, `${label}: ${res.text.slice(0, 300)}`).toBe(200);
      expect(res.text, `${label}: the caller gets the answer`).toContain(answer);
      const row = await rowOf(label, res.requestId);
      expect(row.get("guardrail_bypassed_reason") ?? "", `${label}: nothing was bypassed`).toBe("");
      expect([...row.values()].join("\n"), `${label}: the row carries the answer`).toContain(answer);
    },
  );
});
