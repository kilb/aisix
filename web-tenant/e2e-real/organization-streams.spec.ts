import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "../e2e/stack";
import { login, mintedKey, realCall, realProtocolStream, recorded, seedRealWorld, type StreamProtocol } from "./organization-real-world";

let stack: Stack;
test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
});
test.afterAll(async () => { await stack?.stop(); });
test.afterEach(async ({ page }, info) => {
  if (info.status !== info.expectedStatus) await page.screenshot({ path: info.outputPath("organization-stream-failure.png"), fullPage: true });
});

async function filterCall(page: Page, requestId: string): Promise<ReturnType<Page["locator"]>> {
  const log = page.getByRole("region", { name: "调用记录", exact: true });
  await log.getByLabel("请求 ID", { exact: true }).fill(requestId);
  const rows = log.locator("tbody > tr:not(.usage-detail)");
  await expect(rows).toHaveCount(1);
  await expect(rows.first().getByRole("cell", { name: requestId, exact: true })).toBeVisible();
  return rows.first();
}

async function firstTokenColumn(page: Page): Promise<number> {
  const log = page.getByRole("region", { name: "调用记录", exact: true });
  await expect(log.getByRole("columnheader", { name: "首 token 延迟", exact: true })).toBeVisible();
  const headers = await log.getByRole("columnheader").allTextContents();
  const index = headers.findIndex((header) => header.trim() === "首 token 延迟");
  expect(index, "call table exposes the independent first-token latency column").toBeGreaterThanOrEqual(0);
  return index;
}

test("三种已开放客户流式协议均记录实际首token，非流式保持缺失", async ({ page }) => {
  test.setTimeout(240_000);
  const world = await seedRealWorld(stack);
  const key = await mintedKey(stack, world.api, "三协议首token");
  const protocols: StreamProtocol[] = ["chat", "messages", "responses"];
  const results = await Promise.allSettled(protocols.map((protocol) => realProtocolStream(stack, key.plaintext, world.model.name, protocol)));
  const observations = results.map((result) => {
    if (result.status === "rejected") throw result.reason;
    return result.value;
  });
  const ordinary = await realCall(stack, key.plaintext, world.model.name);
  const calls = await Promise.all(observations.map((observation) => recorded(world.api, observation.requestId)));
  const nonstream = await recorded(world.api, ordinary.requestId);
  for (const call of calls) {
    expect(call.status).toBe("succeeded");
    expect(call).toHaveProperty("first_token_latency_ms");
    expect(call.first_token_latency_ms).not.toBeNull();
    expect(call.first_token_latency_ms).toBeGreaterThan(0);
  }
  expect(nonstream).toHaveProperty("first_token_latency_ms", null);
  await test.info().attach("real-three-protocol-content-and-timing", {
    contentType: "application/json",
    body: Buffer.from(JSON.stringify(observations.map((observation, index) => ({
      ...observation, first_token_latency_ms: calls[index].first_token_latency_ms,
    })), null, 2)),
  });
  await login(page, stack, world.owner);
  await page.getByRole("tab", { name: "用量", exact: true }).click();
  const column = await firstTokenColumn(page);
  for (const call of calls) {
    const row = await filterCall(page, call.request_id);
    await expect(row.getByRole("cell").nth(column)).toHaveText(`${call.first_token_latency_ms} ms`);
  }
  const row = await filterCall(page, ordinary.requestId);
  await expect(row.getByRole("cell").nth(column)).toHaveText("—");
});

test("真实输出后中断流式调用，安全原因与实际已产生用量收费同时可见", async ({ page }) => {
  test.setTimeout(180_000);
  const world = await seedRealWorld(stack);
  const key = await mintedKey(stack, world.api, "真实内容后断开");
  const observation = await realProtocolStream(stack, key.plaintext, world.model.name, "chat", { abortAfterContent: true });
  await test.info().attach("real-client-disconnect-observation", { contentType: "application/json", body: Buffer.from(JSON.stringify(observation, null, 2)) });
  expect(observation.output.length).toBeGreaterThan(0);
  if (!observation.aborted || observation.terminalSeen) {
    throw new Error(`real provider delivered content together with a terminal event before an abort was possible; no midstream cancellation is claimed; request_id=${observation.requestId}, transport_reads=${observation.transportReads}, data_events=${observation.dataEvents}, output_chars=${observation.output.length}`);
  }
  const call = await recorded(world.api, observation.requestId);
  expect(call.status).toBe("failed");
  expect(call.error_class).toBe("client_disconnected");
  expect(call.first_token_latency_ms).toBeGreaterThan(0);
  expect(call.charge.status).toBe("charged");
  expect(call.charge.lines).not.toBeNull();
  const quantities = call.charge.lines as { dimension: string; quantity: number }[];
  expect(quantities.filter((line) => ["output", "reasoning"].includes(line.dimension)).reduce((total, line) => total + line.quantity, 0), "the actually delivered content contributes recorded output usage").toBeGreaterThan(0);
  const calculation = call.charge.calculation;
  expect(BigInt(calculation.final_micro_cny)).toBeGreaterThan(0n);
  expect(BigInt(calculation.original_micro_cny) + BigInt(calculation.adjusted_micro_cny)).toBe(BigInt(calculation.final_micro_cny));
  const ledger = await world.api.get("/api/billing/ledger");
  const entries = ledger.items.filter((entry: Json) => entry.kind === "usage" && entry.event_id === call.event_id);
  expect(entries).toHaveLength(1);
  expect(BigInt(entries[0].amount_micro_cny)).toBe(-BigInt(calculation.original_micro_cny));
  await test.info().attach("real-cancel-charge-and-ledger", {
    contentType: "application/json",
    body: Buffer.from(JSON.stringify({ request_id: call.request_id, event_id: call.event_id, error_class: call.error_class, first_token_latency_ms: call.first_token_latency_ms, charge: call.charge, ledger: entries }, null, 2)),
  });
  await login(page, stack, world.owner);
  await page.getByRole("tab", { name: "用量", exact: true }).click();
  const row = await filterCall(page, observation.requestId);
  await row.getByRole("button", { name: "展开明细", exact: true }).click();
  const log = page.getByRole("region", { name: "调用记录", exact: true });
  await expect(log.getByText("客户端断开连接", { exact: true })).toBeVisible();
  await expect(log.getByText("调用未完成，但已产生用量，按已产生用量计费", { exact: true })).toBeVisible();
  await expect(log.getByText("费用计算", { exact: true })).toBeVisible();
  if (call.charge.estimated) await expect(log.getByText(/估算/).first()).toBeVisible();
  const balance = await world.api.get("/api/billing/balance");
  expect(BigInt(balance.calculation.usage_micro_cny)).toBe(BigInt(calculation.original_micro_cny));
});
