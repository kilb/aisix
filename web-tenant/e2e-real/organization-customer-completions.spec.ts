import { randomUUID } from "node:crypto";
import { expect, test } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "../e2e/stack";
import { mintedKey, seedRealWorld } from "./organization-real-world";

let stack: Stack;
test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
});
test.afterAll(async () => { await stack?.stop(); });

test("客户completions已获授权并到达真实上游，真实404事件与安全错误均保留", async () => {
  test.setTimeout(180_000);
  const world = await seedRealWorld(stack);
  const key = await mintedKey(stack, world.api, "客户legacy真实上游通路");
  const requestId = randomUUID();
  const response = await fetch(`${stack.proxyUrl}/v1/completions`, {
    method: "POST",
    headers: { authorization: `Bearer ${key.plaintext}`, "content-type": "application/json", "x-aisix-request-id": requestId },
    body: JSON.stringify({ model: world.model.name, prompt: "Reply only OK.", max_tokens: 32, stream: true }),
    signal: AbortSignal.timeout(90_000),
  });
  const text = await response.text();
  await test.info().attach("customer-completions-http-status", {
    contentType: "application/json", body: Buffer.from(JSON.stringify({ request_id: requestId, status: response.status })),
  });
  expect(response.status).toBeGreaterThanOrEqual(400);
  expect(response.status).toBeLessThan(600);
  expect(response.status, "customer endpoint authorization must allow the actual legacy upstream attempt").not.toBe(403);
  expect(text.includes("endpoint_forbidden"), "client response must not report an endpoint ACL refusal").toBe(false);
  expect(text.includes(key.plaintext), "client error must not contain the caller credential").toBe(false);
  let answer: Json;
  try {
    answer = JSON.parse(text);
  } catch (error: unknown) {
    if (error instanceof SyntaxError) throw new Error("customer legacy failure must have a standard JSON error shape");
    throw error;
  }
  expect(answer.error).not.toBeNull();
  expect(typeof answer.error).toBe("object");
  expect(typeof answer.error.message).toBe("string");
  expect(answer.error.message.length).toBeGreaterThan(0);
  expect(typeof answer.error.type).toBe("string");
  expect(answer.error.type.length).toBeGreaterThan(0);

  // 唯一final事件必须指向本测试真正配置的上游，单独一个404状态不足以证明通路。
  const query = new URLSearchParams({ team_id: world.tenantId, api_key_id: key.id, request_id: requestId });
  let finals: Json[] = [];
  await expect.poll(async () => {
    const page = await stack.admin.get(`/admin/usage/events?${query}`);
    finals = page.items.filter((item: Json) => item.event.is_final_attempt === true);
    return finals.length;
  }, { timeout: 60_000, message: "the real customer legacy attempt reaches final operator telemetry" }).toBe(1);
  const final = finals[0];
  expect(final).toMatchObject({ team_id: world.tenantId, api_key_id: key.id, request_id: requestId });
  expect(final.event).toHaveProperty("request_id", requestId);
  expect(final.event).toHaveProperty("provider_key_id", world.accountId);
  expect(final.event).toHaveProperty("attempt_index", 0);
  expect(final.event).toHaveProperty("is_final_attempt", true);
  expect(final.event).toHaveProperty("status_code", 404);
  await test.info().attach("customer-completions-real-upstream-attempt", {
    contentType: "application/json",
    body: Buffer.from(JSON.stringify({
      request_id: requestId, event_id: final.event_id, tenant_id: world.tenantId, key_id: key.id,
      provider_key_id: final.event.provider_key_id, attempt_index: final.event.attempt_index,
      upstream_status_code: final.event.status_code, customer_status_code: response.status,
    }, null, 2)),
  });
});
