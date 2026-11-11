import { execFileSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { expect, type Page } from "@playwright/test";
import type { ApiClient, Json, Stack } from "../e2e/stack";

export const PASSWORD = "Organization-real-pass-1111";
export const DISCOUNT_BPS = 1375;
export interface Login { email: string; password: string }
export interface RealWorld {
  tenantId: string; owner: Login; api: ApiClient;
  accountId: string;
  model: { id: string; name: string };
}
interface Catalogue { accountId: string; provider: string; upstreamModel: string; model: { id: string; name: string } }
const catalogues = new WeakMap<Stack, Promise<Catalogue>>();

// 只读授权的生产上游配置；凭据仅驻留当前进程，不写文件、日志或命令参数。
function authorizedProvider(): { api_key: string; api_base: string; provider: string; model: string } {
  const apiBase = process.env.ORG_E2E_PROVIDER_API_BASE;
  const apiKey = process.env.ORG_E2E_PROVIDER_API_KEY;
  const model = process.env.ORG_E2E_PROVIDER_MODEL ?? "gpt-6.1-sol";
  if (apiBase || apiKey) {
    if (!apiBase || !apiKey) throw new Error("both ORG_E2E_PROVIDER_API_BASE and ORG_E2E_PROVIDER_API_KEY are required");
    return { api_key: apiKey, api_base: apiBase, provider: "openai", model };
  }
  const host = process.env.ORG_E2E_PROVIDER_SSH_HOST;
  const key = process.env.ORG_E2E_PROVIDER_KINE_KEY;
  if (!host || !key) throw new Error("ORG_E2E_PROVIDER_SSH_HOST and ORG_E2E_PROVIDER_KINE_KEY are required for the authorized read-only provider");
  if (!/^\/aisix\/[0-9a-f-]+\/provider_keys\/[0-9a-f-]+$/.test(key)) throw new Error("invalid authorized provider key path");
  const query = `SELECT convert_from(value,'UTF8') FROM kine WHERE name='${key}' ORDER BY id DESC LIMIT 1`;
  const quoted = `'${query.replaceAll("'", "'\"'\"'")}'`;
  let raw: string;
  try {
    raw = execFileSync("ssh", ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", host,
      `runuser -u postgres -- psql -X -d kine -At -c ${quoted}`], { encoding: "utf8", timeout: 20_000, stdio: ["ignore", "pipe", "pipe"] });
  } catch { throw new Error("authorized provider configuration could not be read over SSH"); }
  const doc = JSON.parse(raw);
  if (typeof doc.api_key !== "string" || typeof doc.api_base !== "string") throw new Error("authorized provider configuration lacks required fields");
  return { api_key: doc.api_key, api_base: doc.api_base, provider: "rpm", model };
}

async function catalogue(stack: Stack): Promise<Catalogue> {
  let pending = catalogues.get(stack);
  if (!pending) {
    pending = (async () => {
      expect(stack.mockUpstreamUrl, "real acceptance must have no upstream stand-in").toBe("");
      const credential = authorizedProvider();
      const account = await stack.admin.request("POST", "/admin/resources/provider_keys", { doc: {
        display_name: "real-organization-upstream", provider: credential.provider, adapter: "openai",
        api_key: credential.api_key, api_base: credential.api_base, groups: ["shared"],
      } });
      if (account.status !== 201 && account.status !== 200) throw new Error(`isolated provider creation failed: HTTP ${account.status}`);
      const model = await stack.admin.post("/admin/resources/models", { doc: {
        display_name: "organization-real-approved-model", provider: credential.provider, model_name: credential.model, provider_key_id: account.body.id,
      } });
      await stack.admin.post("/admin/pricing/baselines", {
        model_id: model.id, effective_from: new Date(Date.now() - 3000).toISOString(),
        card: { default: { base: { input: "123.456789", output: "234.567891" } } }, note: "真实组织验收价",
      });
      const groups = await stack.admin.get("/admin/provider-groups");
      const shared = groups.items.find((g: Json) => g.name === "shared");
      await stack.admin.post(`/admin/provider-groups/${shared.id}/rates`, {
        model_id: null, cny_per_usd: "1", effective_from: new Date(Date.now() - 3000).toISOString(),
      });
      return { accountId: account.body.id, provider: credential.provider, upstreamModel: credential.model, model: { id: model.id, name: "organization-real-approved-model" } };
    })();
    catalogues.set(stack, pending);
  }
  return pending;
}

export async function seedRealWorld(stack: Stack, options: { credit?: string; creditLimit?: string } = {}): Promise<RealWorld> {
  const { accountId, model } = await catalogue(stack);
  const suffix = randomUUID().slice(0, 8);
  const owner = { email: `owner-${suffix}@organization-real.test`, password: PASSWORD };
  const signup = await stack.tenant().post("/api/signup", { org_name: `真实组织 ${suffix}`, ...owner });
  const tenantId = signup.tenant.id;
  await stack.admin.post(`/admin/tenants/${tenantId}/approve`);
  const current = await stack.admin.get(`/admin/tenants/${tenantId}`);
  await stack.admin.patch(`/admin/tenants/${tenantId}`, { allowed_model_ids: [model.id] }, current.version);
  await stack.admin.post("/admin/pricing/discounts", { tenant_id: tenantId, discount_bps: DISCOUNT_BPS, effective_from: new Date(Date.now() - 3000).toISOString() });
  if ((options.credit ?? "10") !== "0") await stack.admin.post(`/admin/tenants/${tenantId}/credits`, { amount_cny: options.credit ?? "10", reason: "隔离组织真实验收" });
  if (options.creditLimit) await stack.admin.patch(`/admin/tenants/${tenantId}/billing`, { credit_limit_cny: options.creditLimit });
  return { tenantId, owner, api: await signedIn(stack, owner), accountId, model };
}

export async function signedIn(stack: Stack, who: Login): Promise<ApiClient> {
  const api = stack.tenant();
  await api.post("/api/login", who);
  return api;
}
export async function login(page: Page, stack: Stack, who: Login): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(who.email);
  await page.getByLabel("口令", { exact: true }).fill(who.password);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await expect(page.getByRole("button", { name: "登出" })).toBeVisible();
}
export async function mintedKey(stack: Stack, api: ApiClient, label: string, modelIds?: string[]): Promise<{ id: string; plaintext: string }> {
  const r = await api.post("/api/keys", { label, ...(modelIds ? { model_ids: modelIds } : {}) });
  await expect.poll(async () => {
    const res = await fetch(`${stack.proxyUrl}/v1/models`, { headers: { authorization: `Bearer ${r.plaintext}` } });
    await res.arrayBuffer(); return res.status;
  }, { timeout: 60_000 }).toBe(200);
  return { id: r.key.id, plaintext: r.plaintext };
}
export async function grantLaterModel(stack: Stack, world: RealWorld): Promise<{ id: string; name: string }> {
  const c = await catalogue(stack);
  const name = `organization-later-${randomUUID().slice(0, 6)}`;
  const created = await stack.admin.post("/admin/resources/models", { doc: { display_name: name, provider: c.provider, model_name: c.upstreamModel, provider_key_id: c.accountId } });
  const current = await stack.admin.get(`/admin/tenants/${world.tenantId}`);
  await stack.admin.patch(`/admin/tenants/${world.tenantId}`, { allowed_model_ids: [world.model.id, created.id] }, current.version);
  return { id: created.id, name };
}
export async function realCall(stack: Stack, plaintext: string, model: string, stream = false): Promise<{ requestId: string; usage: Json }> {
  const requestId = randomUUID();
  const res = await fetch(`${stack.proxyUrl}/v1/responses`, {
    method: "POST", headers: { authorization: `Bearer ${plaintext}`, "content-type": "application/json", "x-aisix-request-id": requestId },
    body: JSON.stringify({ model, input: "Reply only OK.", max_output_tokens: 32, stream }), signal: AbortSignal.timeout(90_000),
  });
  if (res.status !== 200) { await res.arrayBuffer(); throw new Error(`real upstream request failed HTTP ${res.status}`); }
  let response: Json;
  if (stream) {
    const wire = await res.text();
    const frames = wire.split("\n").filter((line) => line.startsWith("data:") && line.slice(5).trim() !== "[DONE]").map((line) => JSON.parse(line.slice(5)));
    response = frames.find((frame: Json) => frame.type === "response.completed")?.response;
    expect(frames.some((frame: Json) => frame.type === "response.output_text.delta" && frame.delta?.length > 0), "real stream delivers text").toBe(true);
  } else response = await res.json();
  expect(response?.usage?.input_tokens, "provider reports actual input usage").toBeGreaterThan(0);
  expect(response?.usage?.output_tokens, "provider reports actual output usage").toBeGreaterThan(0);
  return { requestId, usage: response.usage };
}
export async function recorded(api: ApiClient, requestId: string): Promise<Json> {
  let call: Json;
  await expect.poll(async () => {
    const calls = await api.get("/api/usage/calls?limit=200");
    call = calls.items.find((item: Json) => item.request_id === requestId);
    return !!call && call.charge?.status !== "pending";
  }, { timeout: 60_000, message: "real request reaches priced call log" }).toBe(true);
  return call;
}

export async function invitedMember(stack: Stack, world: RealWorld, role = "member"): Promise<Login & { id: string }> {
  const email = `${role}-${randomUUID().slice(0, 8)}@organization-real.test`;
  const invite = await stack.admin.post(`/admin/tenants/${world.tenantId}/invites`, { email, role });
  const accepted = await stack.tenant().post("/api/invites/accept", { token: invite.invite_token, password: PASSWORD });
  return { id: accepted.member.id, email, password: PASSWORD };
}
export const COMPLETE_RATES = {
  input: "11", output: "22", cache_read: "0", cache_write: "33", cache_write_1h: "44",
  reasoning: "55", audio_input: "66", audio_output: "77", image_input: "88", image_output: "99", web_search: "11000",
};
export async function fullPriceModel(stack: Stack, world: RealWorld): Promise<{ id: string; name: string }> {
  const c = await catalogue(stack);
  const name = `organization-full-price-model-complete-${randomUUID().slice(0, 8)}`;
  const m = await stack.admin.post("/admin/resources/models", { doc: { display_name: name, provider: c.provider, model_name: c.upstreamModel, provider_key_id: c.accountId } });
  await stack.admin.post("/admin/pricing/baselines", { model_id: m.id, effective_from: new Date(Date.now() - 3000).toISOString(), note: "全维度真实组织报价验收", card: {
    default: { base: COMPLETE_RATES, long_context: [{ above_input_tokens: 200000, rates: { ...COMPLETE_RATES, input: "111", output: "222" } }] },
    batch: { base: { ...COMPLETE_RATES, input: "5.5", output: "11" }, long_context: [{ above_input_tokens: 200000, rates: { ...COMPLETE_RATES, input: "55.5", output: "111" } }] },
  } });
  const current = await stack.admin.get(`/admin/tenants/${world.tenantId}`);
  await stack.admin.patch(`/admin/tenants/${world.tenantId}`, { allowed_model_ids: [world.model.id, m.id] }, current.version);
  return { id: m.id, name };
}

// 金额由独立整数公式计算；输入是实际上游返回的usage，绝不固定token数量。
export function expectedMicroCny(usage: Json): string {
  const weighted = BigInt(usage.input_tokens) * 123456789n + BigInt(usage.output_tokens) * 234567891n;
  const numerator = weighted * BigInt(10000 - DISCOUNT_BPS);
  const denominator = 1000000n * 10000n;
  return ((numerator + denominator - 1n) / denominator).toString();
}

export type StreamProtocol = "chat" | "completions" | "messages" | "responses";
export interface RealStreamObservation {
  requestId: string;
  protocol: StreamProtocol;
  output: string;
  transportReads: number;
  dataEvents: number;
  terminalSeen: boolean;
  aborted: boolean;
}

// 按公开协议读取内容事件；role、created、ping等元数据不会成为输出。
// OpenAI: https://developers.openai.com/api/reference/resources/chat
// https://developers.openai.com/api/reference/resources/completions/methods/create
// https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response_text_delta_event.py
// Anthropic: https://platform.claude.com/docs/en/build-with-claude/streaming
function streamText(protocol: StreamProtocol, event: Json): string {
  if (protocol === "responses") return event.type === "response.output_text.delta" && typeof event.delta === "string" ? event.delta : "";
  if (protocol === "messages") return event.type === "content_block_delta" && event.delta?.type === "text_delta" && typeof event.delta.text === "string" ? event.delta.text : "";
  return (event.choices ?? []).map((choice: Json) => {
    const text = protocol === "completions" ? choice.text : choice.delta?.content;
    return typeof text === "string" ? text : "";
  }).join("");
}
function streamTerminal(protocol: StreamProtocol, event: Json): boolean {
  if (protocol === "responses") return ["response.completed", "response.failed", "response.incomplete"].includes(event.type);
  if (protocol === "messages") return event.type === "message_stop";
  return (event.choices ?? []).some((choice: Json) => choice.finish_reason !== null && choice.finish_reason !== undefined);
}

export async function realProtocolStream(
  stack: Stack,
  plaintext: string,
  model: string,
  protocol: StreamProtocol,
  options: { abortAfterContent?: boolean } = {},
): Promise<RealStreamObservation> {
  const observation: RealStreamObservation = {
    requestId: randomUUID(), protocol, output: "", transportReads: 0, dataEvents: 0, terminalSeen: false, aborted: false,
  };
  const prompt = options.abortAfterContent
    ? "Write the integers from 1 to 500 in order, one per line, each followed by a different short sentence. Start immediately and do not summarize."
    : "Reply only OK.";
  const maxTokens = options.abortAfterContent ? 512 : 32;
  const route = { chat: "/v1/chat/completions", completions: "/v1/completions", messages: "/v1/messages", responses: "/v1/responses" }[protocol];
  const body = protocol === "responses"
    ? { model, input: prompt, max_output_tokens: maxTokens, stream: true }
    : protocol === "completions"
      ? { model, prompt, max_tokens: maxTokens, stream: true }
      : { model, messages: [{ role: "user", content: prompt }], stream: true, ...(protocol === "chat" ? { max_completion_tokens: maxTokens, stream_options: { include_usage: true } } : { max_tokens: maxTokens }) };
  const controller = new AbortController();
  const res = await fetch(`${stack.proxyUrl}${route}`, {
    method: "POST", headers: {
      authorization: `Bearer ${plaintext}`, "content-type": "application/json", "x-aisix-request-id": observation.requestId,
      ...(protocol === "messages" ? { "anthropic-version": "2023-06-01" } : {}),
    },
    body: JSON.stringify(body), signal: AbortSignal.any([controller.signal, AbortSignal.timeout(90_000)]),
  });
  if (res.status !== 200) {
    await res.arrayBuffer();
    throw new Error(`real ${protocol} streaming request failed HTTP ${res.status}; request_id=${observation.requestId}`);
  }
  if (!res.body) throw new Error(`real ${protocol} stream has no readable body`);
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let pending = "";
  try {
    for (;;) {
      const chunk = await reader.read();
      if (chunk.done) break;
      observation.transportReads += 1;
      pending += decoder.decode(chunk.value, { stream: true });
      const frames = pending.split(/\r?\n\r?\n/);
      pending = frames.pop() ?? "";
      // 同一次网络读取内先检查全部终态，避免收到DONE后假装中途断开。
      for (const frame of frames) {
        const data = frame.split(/\r?\n/).filter((line) => line.startsWith("data:")).map((line) => line.slice(5).trimStart()).join("\n");
        if (!data) continue;
        observation.dataEvents += 1;
        if (data.trim() === "[DONE]") { observation.terminalSeen = true; continue; }
        const event = JSON.parse(data);
        observation.output += streamText(protocol, event);
        observation.terminalSeen ||= streamTerminal(protocol, event);
      }
      if (options.abortAfterContent && observation.output.length > 0) {
        if (!observation.terminalSeen) {
          controller.abort();
          observation.aborted = true;
          await reader.cancel().catch((error: unknown) => {
            if (!(error instanceof DOMException && error.name === "AbortError")) throw error;
          });
          break;
        }
      }
    }
  } finally {
    reader.releaseLock();
  }
  expect(observation.output.length, `real ${protocol} delivers actual nonempty text`).toBeGreaterThan(0);
  if (!options.abortAfterContent) expect(observation.terminalSeen, `real ${protocol} reaches a protocol terminal event`).toBe(true);
  return observation;
}
