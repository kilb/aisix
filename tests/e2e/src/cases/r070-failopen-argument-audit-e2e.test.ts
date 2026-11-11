import { createHash, randomUUID } from "node:crypto";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, test } from "vitest";
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

// R-070：深层 arguments 虽是合法 JSON，结构化检查仍无法读取。显式 fail-open
// 或 monitor 必须继续服务，并记录 bypass；full-content 导出不能把它当已检查内容。
// 判别力：移除 moderation 的 bypass 合并后，Block 丢 audit、Block/Mask 均泄漏
// capture；浅层及相反 hook 对照保证导出通路正常，也没有误报 bypass。
// 供应商工具调用 wire 依据官方 SDK：
// https://github.com/openai/openai-python/blob/main/src/openai/types/chat/chat_completion_message_function_tool_call.py
// https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response_function_tool_call.py

const KEY = "sk-r070-unreadable-arguments";
const LOGSTORE = "r070-argument-audit";
const ESCAPED_EMAIL = "audit\\u0040example.com";
const OUTPUT_MARKER = "r070-answer-arguments";
const CLEAN_ARGUMENTS = '{"to":"plain-safe-name"}';
const DEEP_ARGUMENTS = `{"padding":${"[".repeat(130)}0${"]".repeat(130)},"to":"${ESCAPED_EMAIL}","note":"r070-unread-argument-content"}`;

type Json = Record<string, unknown>;
type Surface = "chat" | "responses";
type Side = "input" | "output";
type Action = "block" | "mask";
type Mode = "open" | "monitor" | "closed";

const modelName = (action: Action, hook: Side, mode: Mode) => `r070-${action}-${hook}-${mode}`;

async function startUpstream(): Promise<{
  baseUrl: string;
  requests: Json[];
  close(): Promise<void>;
}> {
  const requests: Json[] = [];
  const server: Server = createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (chunk: Buffer) => chunks.push(chunk));
    req.on("end", () => {
      const body = JSON.parse(Buffer.concat(chunks).toString("utf8")) as Json;
      requests.push(body);
      const output = JSON.stringify(body).includes(OUTPUT_MARKER);
      const model = String(body.model);
      res.setHeader("content-type", "application/json");
      if (req.url === "/v1/responses") {
        res.end(JSON.stringify({
          id: `resp_${randomUUID()}`,
          object: "response",
          created_at: 0,
          status: "completed",
          model,
          output: output ? [{
            type: "function_call", id: "fc_r070", call_id: "call_r070",
            name: "send", arguments: DEEP_ARGUMENTS, status: "completed",
          }] : [{
            type: "message", id: "msg_r070", role: "assistant", status: "completed",
            content: [{ type: "output_text", text: "screened-answer", annotations: [] }],
          }],
          usage: { input_tokens: 5, output_tokens: 1, total_tokens: 6 },
        }));
        return;
      }
      res.end(JSON.stringify({
        id: `chatcmpl-${randomUUID()}`,
        object: "chat.completion",
        created: 0,
        model,
        choices: [{
          index: 0,
          message: output ? {
            role: "assistant", content: null,
            tool_calls: [{
              id: "call_r070", type: "function",
              function: { name: "send", arguments: DEEP_ARGUMENTS },
            }],
          } : { role: "assistant", content: "screened-answer" },
          finish_reason: output ? "tool_calls" : "stop",
        }],
        usage: { prompt_tokens: 5, completion_tokens: 1, total_tokens: 6 },
      }));
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    requests,
    close: async () => {
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

function requestBody(surface: Surface, model: string, inputArguments: string | null, output: boolean, requestId: string, readableHit: boolean): Json {
  const prompt = `${output ? OUTPUT_MARKER : "safe-prompt"}-${requestId}${readableHit ? " contact visible@example.com" : ""}`;
  if (surface === "responses") {
    return {
      model,
      input: inputArguments === null ? prompt : [
        { type: "function_call", call_id: "call_input", name: "send", arguments: inputArguments },
        { type: "function_call_output", call_id: "call_input", output: "sent" },
        { role: "user", content: prompt },
      ],
    };
  }
  return {
    model,
    messages: inputArguments === null ? [{ role: "user", content: prompt }] : [
      { role: "user", content: "send the note" },
      { role: "assistant", content: null, tool_calls: [{
        id: "call_input", type: "function", function: { name: "send", arguments: inputArguments },
      }] },
      { role: "tool", tool_call_id: "call_input", content: "sent" },
      { role: "user", content: prompt },
    ],
  };
}

describe("R-070: unreadable fail-open arguments are allowed, audited and excluded from full capture", () => {
  let etcdReachable = false;
  let app: SpawnedApp | undefined;
  let sls: MockSls | undefined;
  let upstream: Awaited<ReturnType<typeof startUpstream>> | undefined;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    [sls, upstream] = await Promise.all([startMockSls(), startUpstream()]);
    app = await spawnApp({
      extraEnv: { SLS_CRED_MOCK_AK_ID: "mock-akid", SLS_CRED_MOCK_AK_SECRET: "mock-secret" },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: "r070-full", enabled: true, kind: "aliyun_sls", endpoint: sls.url,
      project: "aisix-e2e-obs", logstore: LOGSTORE, credential_ref: "mock", content_mode: "full",
    });
    const key = await seed.createProviderKey({
      display_name: "r070-upstream", secret: "sk-r070-fixture", api_base: `${upstream.baseUrl}/v1`,
    });
    await Promise.all((["block", "mask"] as const).flatMap((action) =>
      (["input", "output"] as const).flatMap((hook) =>
        (["open", "monitor", "closed"] as const).map(async (mode) => {
          const name = modelName(action, hook, mode);
          const guardrail = await seed.createGuardrail({
            name, enabled: true, kind: "pii", hook_point: hook,
            fail_open: mode === "open", enforcement_mode: mode === "monitor" ? "monitor" : "block",
            detectors: [{ type: "email", action }],
          }, { attach: false });
          const model = await seed.createModel({
            display_name: name, provider: "openai", model_name: "gpt-4o-mini", provider_key_id: key.id,
          });
          await seed.attachGuardrailToModel(guardrail.id as string, model.id as string);
        }),
      ),
    ));
    // 最后写入 key；认证成功证明全部模型、护栏及 exporter 已到达同一快照。
    await seed.createApiKey({
      key_hash: createHash("sha256").update(KEY).digest("hex"), allowed_models: ["*"],
    });
    await waitConfigPropagation(async () => {
      const response = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${KEY}` },
      });
      await response.arrayBuffer();
      if (response.status === 401) return false;
      if (response.status !== 200) throw new Error(`model readiness returned ${response.status}`);
      return true;
    });
  }, 120_000);

  afterAll(async () => {
    await app?.exit();
    await Promise.all([sls?.close(), upstream?.close()]);
  });

  async function send(surface: Surface, model: string, args: string | null, output: boolean, readableHit = false) {
    const requestId = `r070-${randomUUID()}`;
    const response = await fetch(`${app!.proxyUrl}/v1/${surface === "chat" ? "chat/completions" : "responses"}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${KEY}`, "content-type": "application/json", "x-aisix-request-id": requestId,
      },
      body: JSON.stringify(requestBody(surface, model, args, output, requestId, readableHit)),
    });
    const text = await response.text();
    return {
      status: response.status, text, requestId,
      sent: upstream!.requests.filter((body) => JSON.stringify(body).includes(requestId)),
    };
  }

  const rowFor = (requestId: string) => waitForSlsLog(
    sls!, LOGSTORE, (row) => row.get("request_id") === requestId, requestId, 20_000,
  );

  for (const surface of ["chat", "responses"] as const) {
    test.concurrent(`${surface}: a located readable block hit does not authorize exporting unread arguments`, async (ctx) => {
      if (!etcdReachable) return ctx.skip();
      const { expect } = ctx;
      const result = await send(surface, modelName("block", "input", "closed"), DEEP_ARGUMENTS, false, true);
      expect(result.status, result.text).toBe(422);
      expect(result.sent).toHaveLength(0);
      const row = await rowFor(result.requestId);
      expect(row.get("guardrail_blocked")).toBe("true");
      expect.soft([...row.values()].join("\n")).not.toContain("r070-unread-argument-content");
      expect.soft([...row.values()].join("\n")).not.toContain("visible@example.com");
      expect.soft(row.get("input_messages")).toBeUndefined();
    }, 30_000);

    test.concurrent(`${surface}: an ordinary readable block still exports its safely masked body`, async (ctx) => {
      if (!etcdReachable) return ctx.skip();
      const { expect } = ctx;
      const result = await send(surface, modelName("block", "input", "closed"), CLEAN_ARGUMENTS, false, true);
      expect(result.status, result.text).toBe(422);
      expect(result.sent).toHaveLength(0);
      const row = await rowFor(result.requestId);
      expect(row.get("guardrail_blocked")).toBe("true");
      expect(row.get("input_messages")).toContain("[EMAIL_REDACTED]");
      expect(row.get("input_messages")).toContain("plain-safe-name");
      expect([...row.values()].join("\n")).not.toContain("visible@example.com");
    }, 30_000);

    for (const action of ["block", "mask"] as const) {
      for (const hook of ["input", "output"] as const) {
        for (const mode of ["open", "monitor"] as const) {
          test.concurrent(`${surface} ${action} ${hook} ${mode}: allows unread arguments but audits and withholds content`, async (ctx) => {
            if (!etcdReachable) return ctx.skip();
            const { expect } = ctx;
            const result = await send(surface, modelName(action, hook, mode), hook === "input" ? DEEP_ARGUMENTS : null, hook === "output");
            expect(result.status, result.text).toBe(200);
            expect(result.sent).toHaveLength(1);
            if (hook === "input") {
              expect(JSON.stringify(result.sent)).toContain("r070-unread-argument-content");
            } else {
              expect(result.text).toContain("r070-unread-argument-content");
            }
            const row = await rowFor(result.requestId);
            expect.soft(row.get("status_code")).toBe("200");
            expect.soft(row.get("guardrail_bypassed_reason")).toBe("unscannable_body");
            expect.soft(row.get("guardrail_blocked")).not.toBe("true");
            expect.soft([...row.values()].join("\n")).not.toContain("r070-unread-argument-content");
            expect.soft(row.get(hook === "input" ? "input_messages" : "output_messages")).toBeUndefined();
          }, 30_000);
        }

        test.concurrent(`${surface} ${action} ${hook}: fail-closed still refuses unread arguments`, async (ctx) => {
          if (!etcdReachable) return ctx.skip();
          const { expect } = ctx;
          const result = await send(surface, modelName(action, hook, "closed"), hook === "input" ? DEEP_ARGUMENTS : null, hook === "output");
          expect(result.status, result.text).toBe(422);
          expect(result.sent).toHaveLength(hook === "input" ? 0 : 1);
          const row = await rowFor(result.requestId);
          expect(row.get("guardrail_blocked")).toBe("true");
          expect([...row.values()].join("\n")).not.toContain("r070-unread-argument-content");
        }, 30_000);

        test.concurrent(`${surface} ${action} ${hook}: an unread opposite hook is neither a bypass nor hidden capture`, async (ctx) => {
          if (!etcdReachable) return ctx.skip();
          const { expect } = ctx;
          const result = await send(surface, modelName(action, hook, "closed"), hook === "output" ? DEEP_ARGUMENTS : null, hook === "input");
          expect(result.status, result.text).toBe(200);
          expect(result.sent).toHaveLength(1);
          const row = await rowFor(result.requestId);
          expect(row.get("guardrail_bypassed_reason") ?? "").toBe("");
          expect([...row.values()].join("\n")).toContain("r070-unread-argument-content");
        }, 30_000);
      }

      test.concurrent(`${surface} ${action}: a readable clean argument is still captured`, async (ctx) => {
        if (!etcdReachable) return ctx.skip();
        const { expect } = ctx;
        const result = await send(surface, modelName(action, "input", "closed"), CLEAN_ARGUMENTS, false);
        expect(result.status, result.text).toBe(200);
        expect(result.sent).toHaveLength(1);
        const row = await rowFor(result.requestId);
        expect(row.get("guardrail_bypassed_reason") ?? "").toBe("");
        expect(row.get("input_messages")).toContain("plain-safe-name");
      }, 30_000);
    }
  }
});
