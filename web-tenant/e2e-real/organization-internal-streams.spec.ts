import { randomUUID } from "node:crypto";
import { expect, test } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "../e2e/stack";
import { mintedKey, realProtocolStream, seedRealWorld } from "./organization-real-world";

let stack: Stack;
test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
});
test.afterAll(async () => { await stack?.stop(); });

async function finalEvent(teamId: string, keyId: string, requestId: string): Promise<Json> {
  let matches: Json[] = [];
  const query = new URLSearchParams({ team_id: teamId, api_key_id: keyId, request_id: requestId });
  await expect.poll(async () => {
    const page = await stack.admin.get(`/admin/usage/events?${query}`);
    matches = page.items.filter((item: Json) => item.event.is_final_attempt === true);
    return matches.length;
  }, { timeout: 60_000, message: "the real internal request reaches final telemetry through the gateway" }).toBe(1);
  expect(matches[0]).toMatchObject({ team_id: teamId, api_key_id: keyId, request_id: requestId });
  expect(matches[0].event.status_code).toBe(200);
  return matches[0];
}

test("内部与客户密钥真实completions均可访问，内部首内容计时与非流式缺失", async () => {
  test.setTimeout(240_000);
  const world = await seedRealWorld(stack);
  const tenants = await stack.admin.get("/admin/tenants");
  const internal = tenants.items.filter((tenant: Json) => tenant.is_internal === true);
  expect(internal).toHaveLength(1);
  expect(internal[0].status).toBe("active");
  const groups = await stack.admin.get("/admin/provider-groups");
  const group = groups.items.find((item: Json) => item.name === "shared");
  expect(group).toBeDefined();
  const minted = await stack.admin.post(`/admin/tenants/${internal[0].id}/keys`, {
    label: "隔离内部completions实流验收", group_id: group.id, allowed_models: [world.model.name],
  });
  const internalKeyId: string = minted.key.id;
  const internalPlaintext: string = minted.plaintext;
  await expect.poll(async () => {
    const response = await fetch(`${stack.proxyUrl}/v1/models`, { headers: { authorization: `Bearer ${internalPlaintext}` } });
    if (response.status !== 200) { await response.arrayBuffer(); return false; }
    const catalogue = await response.json();
    return catalogue.data?.some((model: { id: string }) => model.id === world.model.name) ?? false;
  }, { timeout: 60_000, message: "the internally authorized model and key reach the real gateway" }).toBe(true);

  // 本专门入口要求真实上游支持legacy协议，客户与内部密钥都必须交付实际内容。
  const customerKey = await mintedKey(stack, world.api, "客户completions实流验收");
  const customerStream = await realProtocolStream(stack, customerKey.plaintext, world.model.name, "completions");
  expect(customerStream.output.length).toBeGreaterThan(0);
  expect(customerStream.terminalSeen).toBe(true);

  const streamed = await realProtocolStream(stack, internalPlaintext, world.model.name, "completions");
  expect(streamed.output.length).toBeGreaterThan(0);
  expect(streamed.terminalSeen).toBe(true);
  const streamEvent = await finalEvent(internal[0].id, internalKeyId, streamed.requestId);
  expect(Object.hasOwn(streamEvent.event, "first_token_latency_ms")).toBe(true);
  expect(streamEvent.event.first_token_latency_ms).toBeGreaterThan(0);
  expect(streamEvent.event.completion_tokens, "real completions content has recorded output usage").toBeGreaterThan(0);

  const requestId = randomUUID();
  const ordinary = await fetch(`${stack.proxyUrl}/v1/completions`, {
    method: "POST", headers: { authorization: `Bearer ${internalPlaintext}`, "content-type": "application/json", "x-aisix-request-id": requestId },
    body: JSON.stringify({ model: world.model.name, prompt: "Reply only OK.", max_tokens: 32, stream: false }),
    signal: AbortSignal.timeout(90_000),
  });
  if (ordinary.status !== 200) {
    await ordinary.arrayBuffer();
    throw new Error(`real internal nonstream completions failed HTTP ${ordinary.status}; request_id=${requestId}`);
  }
  const answer = await ordinary.json();
  expect(answer.choices?.some((choice: Json) => typeof choice.text === "string" && choice.text.length > 0), "real nonstream completion contains generated text").toBe(true);
  expect(answer.usage?.completion_tokens).toBeGreaterThan(0);
  const ordinaryEvent = await finalEvent(internal[0].id, internalKeyId, requestId);
  expect(Object.hasOwn(ordinaryEvent.event, "first_token_latency_ms")).toBe(false);
  await test.info().attach("real-internal-and-customer-completions-timing", {
    contentType: "application/json",
    body: Buffer.from(JSON.stringify({
      stream: { ...streamed, event_id: streamEvent.event_id, first_token_latency_ms: streamEvent.event.first_token_latency_ms },
      nonstream: { request_id: requestId, event_id: ordinaryEvent.event_id, output_tokens: answer.usage.completion_tokens, first_token_field_present: Object.hasOwn(ordinaryEvent.event, "first_token_latency_ms") },
      customer_stream: customerStream,
    }, null, 2)),
  });
});
