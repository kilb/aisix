import { randomUUID } from "node:crypto";
import { expect, test, type Locator } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "../e2e/stack";
import { invitedMember, login, mintedKey, realCall, realProtocolStream, recorded, seedRealWorld, signedIn } from "./organization-real-world";

let stack: Stack;
test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
});
test.afterAll(async () => { await stack?.stop(); });
test.afterEach(async ({ page }, info) => {
  if (info.status !== info.expectedStatus) await page.screenshot({ path: info.outputPath("api-metrics-failure.png"), fullPage: true });
});

function counter(event: Json, name: string): number {
  // 已冻结的UsageEvent计数为u32且零值省略；这里只求已记录数量，不声称provider报告覆盖率。
  if (event[name] === undefined) return 0;
  expect(Number.isSafeInteger(event[name]), `${name} is a recorded integer`).toBe(true);
  expect(event[name]).toBeGreaterThanOrEqual(0);
  return event[name];
}

async function chat(plaintext: string, model: string): Promise<{ requestId: string; usage: Json }> {
  const requestId = randomUUID();
  const response = await fetch(`${stack.proxyUrl}/v1/chat/completions`, {
    method: "POST", headers: { authorization: `Bearer ${plaintext}`, "content-type": "application/json", "x-aisix-request-id": requestId },
    body: JSON.stringify({ model, messages: [{ role: "user", content: "Reply only OK." }], max_completion_tokens: 32 }),
    signal: AbortSignal.timeout(90_000),
  });
  expect(response.status, "real Chat Completions succeeds").toBe(200);
  const answer = await response.json();
  expect(answer).toHaveProperty("usage.prompt_tokens");
  expect(answer).toHaveProperty("usage.completion_tokens");
  expect(answer.usage.prompt_tokens).toBeGreaterThan(0);
  expect(answer.usage.completion_tokens).toBeGreaterThan(0);
  return { requestId, usage: answer.usage };
}

async function definition(card: Locator, label: string, value: string): Promise<void> {
  const term = card.locator("dt").filter({ hasText: new RegExp(`^${label.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}$`) });
  await expect(term).toHaveCount(1);
  await expect(term.locator("..").locator("dd")).toHaveText(value);
}

function duration(value: number): string {
  return `${new Intl.NumberFormat("en-US", { maximumFractionDigits: 2 }).format(value < 1000 ? value : value / 1000)} ${value < 1000 ? "ms" : "s"}`;
}

function percent(value: number): string {
  return `${new Intl.NumberFormat("en-US", { maximumFractionDigits: 1 }).format(value * 100)}%`;
}

function money(value: bigint): string {
  const negative = value < 0n;
  const absolute = negative ? -value : value;
  const integer = (absolute / 1_000_000n).toString().replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  const fraction = (absolute % 1_000_000n).toString().padStart(6, "0").replace(/0+$/, "").padEnd(2, "0");
  return `${negative ? "-" : ""}¥${integer}.${fraction}`;
}

test("真实请求、响应缓存与中断用量完整聚合，统计分母和成员隔离正确", async ({ page }) => {
  test.setTimeout(300_000);
  const world = await seedRealWorld(stack);
  const member = await invitedMember(stack, world);
  const unusedMember = await invitedMember(stack, world);
  const memberApi = await signedIn(stack, member);
  const unusedApi = await signedIn(stack, unusedMember);
  const outsider = await seedRealWorld(stack);
  await stack.admin.post("/admin/resources/cache_policies", { doc: {
    name: `metrics-cache-${world.tenantId.slice(0, 8)}`, scope: "team", applies_to_model_id: world.model.id, ttl_seconds: 300,
  } });
  // 策略先于这两把新密钥投影；模型就绪检查不消费模型token。
  const ownerKey = await mintedKey(stack, world.api, "指标非流式与响应缓存");
  const memberKey = await mintedKey(stack, memberApi, "指标流式与中断");
  const startedAt = Date.now();
  const results = await Promise.allSettled([
    chat(ownerKey.plaintext, world.model.name),
    realCall(stack, memberKey.plaintext, world.model.name, true),
    realProtocolStream(stack, memberKey.plaintext, world.model.name, "chat", { abortAfterContent: true }),
  ]);
  const errors = results.filter((result) => result.status === "rejected");
  if (errors.length) throw errors[0].reason;
  const first = (results[0] as PromiseFulfilledResult<Awaited<ReturnType<typeof chat>>>).value;
  const streamed = (results[1] as PromiseFulfilledResult<Awaited<ReturnType<typeof realCall>>>).value;
  const interrupted = (results[2] as PromiseFulfilledResult<Awaited<ReturnType<typeof realProtocolStream>>>).value;
  expect(interrupted.output.length).toBeGreaterThan(0);
  if (!interrupted.aborted || interrupted.terminalSeen) {
    throw new Error("real provider finished before a midstream abort was possible; no partial failure is claimed");
  }
  const firstRow = await recorded(world.api, first.requestId);
  const repeated = await chat(ownerKey.plaintext, world.model.name);
  const repeatedRow = await recorded(world.api, repeated.requestId);
  const streamRow = await recorded(memberApi, streamed.requestId);
  const interruptedRow = await recorded(memberApi, interrupted.requestId);
  expect(firstRow.status).toBe("succeeded");
  expect(repeatedRow.status).toBe("succeeded");
  expect(streamRow.status).toBe("succeeded");
  expect(interruptedRow).toMatchObject({ status: "failed", error_class: "client_disconnected" });
  expect(repeatedRow).toHaveProperty("cache_hit", true);
  expect(firstRow).toHaveProperty("first_token_latency_ms", null);
  expect(repeatedRow).toHaveProperty("first_token_latency_ms", null);
  for (const call of [streamRow, interruptedRow]) {
    expect(call).toHaveProperty("first_token_latency_ms");
    expect(Number.isSafeInteger(call.first_token_latency_ms)).toBe(true);
    expect(call.first_token_latency_ms).toBeGreaterThanOrEqual(0);
  }

  const from = new Date(Math.floor(startedAt / 60_000) * 60_000 - 120_000).toISOString();
  const to = new Date(Math.floor(Date.now() / 60_000) * 60_000 + 120_000).toISOString();
  const query = new URLSearchParams({ from, to, bucket: "minute" });
  let metrics: Json;
  await expect.poll(async () => {
    metrics = await world.api.get(`/api/usage/metrics?${query}`);
    expect(metrics).toHaveProperty("totals.requests");
    return metrics.totals.requests;
  }, { timeout: 60_000, message: "all four real final requests enter full-range aggregates" }).toBe(4);
  expect(Date.parse(metrics.from)).toBe(Date.parse(from));
  expect(Date.parse(metrics.to)).toBe(Date.parse(to));
  expect(metrics).toMatchObject({ bucket: "minute", totals: {
    requests: 4, succeeded_requests: 3, failed_requests: 1, known_outcome_requests: 4,
    unknown_outcome_requests: 0, unknown_final_attempts: 0, success_rate: 0.75,
    billing_attempts: 4, unpriced_attempts: 0, adjusted_micro_cny: "0",
    first_token_latency_ms: { sample_count: 2, missing_count: 2 },
  } });
  const latencies = [streamRow.first_token_latency_ms, interruptedRow.first_token_latency_ms].sort((a, b) => a - b);
  expect(metrics.totals.first_token_latency_ms.avg).toBeCloseTo((latencies[0] + latencies[1]) / 2, 8);
  expect(metrics.totals.first_token_latency_ms.p50).toBeCloseTo((latencies[0] + latencies[1]) / 2, 8);
  expect(metrics.totals.first_token_latency_ms.p95).toBeCloseTo(latencies[0] + (latencies[1] - latencies[0]) * 0.95, 8);
  for (const field of ["first_token_latency_ms", "response_latency_ms", "attempt_duration_ms"]) {
    expect(metrics.totals[field], `${field} exposes a measured or explicitly unknown P99`).toHaveProperty("p99");
  }
  expect(metrics.totals.first_token_latency_ms.p99).toBeCloseTo(latencies[0] + (latencies[1] - latencies[0]) * 0.99, 8);

  const raw = await stack.admin.get(`/admin/usage/events?${new URLSearchParams({ team_id: world.tenantId, limit: "200" })}`);
  expect(Array.isArray(raw.items)).toBe(true);
  const finals = raw.items.filter((item: Json) => item.event.is_final_attempt === true);
  expect(finals).toHaveLength(4);
  expect(new Set(finals.map((item: Json) => item.request_id))).toEqual(new Set([first.requestId, repeated.requestId, streamed.requestId, interrupted.requestId]));
  const expectedTokens = Object.fromEntries(["prompt_tokens", "completion_tokens", "reasoning_tokens", "cached_prompt_tokens", "cache_read_tokens", "cache_creation_tokens"].map((name) => [name, finals.reduce((sum: number, item: Json) => sum + counter(item.event, name), 0)]));
  expect(expectedTokens.prompt_tokens).toBeGreaterThan(0);
  expect(expectedTokens.completion_tokens).toBeGreaterThan(0);
  expect(metrics.totals.tokens).toMatchObject(expectedTokens);
  const input = expectedTokens.prompt_tokens + expectedTokens.cache_read_tokens + expectedTokens.cache_creation_tokens;
  const hitTokens = expectedTokens.cached_prompt_tokens + expectedTokens.cache_read_tokens;
  expect(metrics.totals.tokens.input_tokens).toBe(input);
  expect(metrics.totals.tokens.cache_hit_tokens).toBe(hitTokens);
  expect(metrics.totals.tokens.cache_hit_rate).toBeCloseTo(hitTokens / input, 10);
  expect(metrics.totals.response_cache.hit_requests).toBe(1);
  const cache = metrics.totals.response_cache;
  expect(cache.hit_requests + cache.miss_requests + cache.bypass_requests + cache.disabled_requests + cache.unknown_requests).toBe(4);
  expect(cache.hit_rate).toBeCloseTo(cache.hit_requests / (cache.hit_requests + cache.miss_requests), 10);

  const ledger = await world.api.get("/api/billing/ledger");
  expect(Array.isArray(ledger.items)).toBe(true);
  const events = new Set(finals.map((item: Json) => item.event_id));
  const debits = ledger.items.filter((entry: Json) => entry.kind === "usage" && events.has(entry.event_id));
  expect(debits).toHaveLength(3);
  expect(debits.some((entry: Json) => entry.event_id === repeatedRow.event_id)).toBe(false);
  expect(debits.some((entry: Json) => entry.event_id === interruptedRow.event_id)).toBe(true);
  const expectedFee = debits.reduce((sum: bigint, entry: Json) => sum - BigInt(entry.amount_micro_cny), 0n);
  expect(expectedFee).toBeGreaterThan(0n);
  expect(metrics.totals.charged_micro_cny).toBe(expectedFee.toString());
  expect(metrics.series.length).toBeGreaterThanOrEqual(4);
  expect(metrics.series.filter((row: Json) => row.requests === 0).length).toBeGreaterThanOrEqual(2);
  expect(metrics.series.reduce((sum: number, row: Json) => sum + row.requests, 0)).toBe(4);
  expect(metrics.series.reduce((sum: bigint, row: Json) => sum + BigInt(row.charged_micro_cny), 0n)).toBe(expectedFee);
  expect(metrics.models).toHaveLength(1);
  expect(metrics.models[0]).toMatchObject({ model: { id: world.model.id, name: world.model.name }, requests: 4 });
  for (const value of [ownerKey.plaintext, memberKey.plaintext, world.accountId]) expect(JSON.stringify(metrics)).not.toContain(value);

  const adminMetrics = await stack.admin.get(`/admin/usage/metrics?${query}&tenant_id=${world.tenantId}`);
  expect(adminMetrics).toEqual(metrics);
  const limited = await world.api.get("/api/usage/calls?limit=1");
  expect(limited.items).toHaveLength(1);
  expect((await world.api.get(`/api/usage/metrics?${query}`)).totals).toEqual(metrics.totals);
  const own = await memberApi.get(`/api/usage/metrics?${query}`);
  expect(own.totals).toMatchObject({ requests: 2, succeeded_requests: 1, failed_requests: 1, success_rate: 0.5, first_token_latency_ms: { sample_count: 2, missing_count: 0 } });
  expect((await world.api.get(`/api/usage/metrics?${query}&member_id=${member.id}`)).totals).toEqual(own.totals);
  for (const api of [unusedApi, outsider.api]) {
    const empty = await api.get(`/api/usage/metrics?${query}`);
    expect(empty.totals).toMatchObject({ requests: 0, known_outcome_requests: 0, success_rate: null, charged_micro_cny: "0", first_token_latency_ms: { sample_count: 0, missing_count: 0, avg: null, p99: null } });
    expect(empty.models).toEqual([]);
  }
  const narrowed = await memberApi.get(`/api/usage/metrics?${query}&api_key_id=${ownerKey.id}`);
  expect(narrowed.totals.requests).toBe(0);
  expect(narrowed.models).toEqual([]);

  await page.setViewportSize({ width: 1440, height: 1000 });
  await login(page, stack, world.owner);
  const displayedMetricsAnswer = page.waitForResponse((response) => new URL(response.url()).pathname.endsWith("/api/usage/metrics") && response.status() === 200);
  await page.getByRole("tab", { name: "用量", exact: true }).click();
  const displayedMetrics = await (await displayedMetricsAnswer).json();
  const health = page.getByRole("group", { name: "请求成功率摘要", exact: true });
  await expect(health).toBeVisible();
  await definition(health, "成功率", "75%");
  await definition(health, "成功", "3");
  await definition(health, "失败", "1");
  await definition(health, "未知", "0");
  await expect(health.getByRole("img", { name: "最终请求构成：成功 3，失败 1，未知 0，共 4", exact: true })).toBeVisible();
  await page.getByRole("button", { name: "请求", exact: true }).click();
  const panel = page.getByRole("region", { name: "完整 API 指标", exact: true });
  await expect(panel).toBeVisible();
  await expect(panel.getByText("最终请求", { exact: true })).toBeVisible();
  await expect(panel.getByText("失败率（已知结果）", { exact: true })).toBeVisible();
  const currentCard = (group: string) => panel.getByRole("heading", { name: `API 指标 · ${group}`, exact: true }).locator("xpath=ancestor::section[1]");
  let card = currentCard("请求");
  await expect(card.getByText("最终请求", { exact: true }).locator("..").getByText("4", { exact: true })).toBeVisible();
  await definition(card, "成功率", "75%");
  await definition(card, "失败率（已知结果）", "25%");
  await definition(card, "结果已知的请求", "4");
  await definition(card, "成功请求", "3");
  await definition(card, "失败请求", "1");
  const capture = async (name: string): Promise<void> => {
    const bounds = await panel.boundingBox();
    expect(bounds).not.toBeNull();
    await page.setViewportSize({ width: 1440, height: Math.max(1000, Math.ceil(bounds!.height) + 240) });
    await panel.scrollIntoViewIfNeeded();
    const path = test.info().outputPath(`metrics-${name}-1440.png`);
    await panel.screenshot({ path });
    await test.info().attach(`metrics-${name}-1440`, { path, contentType: "image/png" });
  };
  await capture("requests");
  const chartButton = card.getByRole("button", { name: /按左右方向键切换期间$/ });
  await expect(chartButton).toHaveAccessibleName("查看各期数据，按左右方向键切换期间");
  for (let index = 0; index < 120 && !await chartButton.evaluate((element) => element === document.activeElement); index += 1) await page.keyboard.press("Tab");
  expect(await chartButton.evaluate((element) => element === document.activeElement)).toBe(true);
  const points = displayedMetrics.series;
  const pointDay = (iso: string): string => {
    const parts = new Intl.DateTimeFormat("en-CA", { timeZone: "Asia/Shanghai", year: "numeric", month: "2-digit", day: "2-digit" }).formatToParts(new Date(iso));
    const part = (name: string) => parts.find(value => value.type === name)?.value;
    return `${part("year")}-${part("month")}-${part("day")}`;
  };
  const pointText = (index: number): string => `${pointDay(points[index].period_start)} · ${new Intl.NumberFormat("en-US").format(points[index].requests)}`;
  await expect(card.getByText(pointText(points.length - 1), { exact: true })).toBeVisible();
  await page.keyboard.press("Home");
  await expect(card.getByText(pointText(0), { exact: true })).toBeVisible();
  await page.keyboard.press("End");
  await page.keyboard.press("ArrowLeft");
  await expect(card.getByText(pointText(points.length - 2), { exact: true })).toBeVisible();
  await page.keyboard.press("ArrowRight");
  await expect(card.getByText(pointText(points.length - 1), { exact: true })).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(card.getByText(pointText(points.length - 1), { exact: true })).toHaveCount(0);
  await panel.getByRole("button", { name: "延迟", exact: true }).click();
  card = currentCard("延迟");
  await card.getByRole("combobox", { name: "耗时口径", exact: true }).selectOption({ label: "首 token 延迟" });
  await definition(card, "P50", duration((latencies[0] + latencies[1]) / 2));
  await definition(card, "P95", duration(latencies[0] + (latencies[1] - latencies[0]) * 0.95));
  await definition(card, "P99", duration(latencies[0] + (latencies[1] - latencies[0]) * 0.99));
  await definition(card, "实测样本", "2");
  await definition(card, "无实测值", "2");
  await capture("latency");
  for (const label of ["响应延迟", "上游尝试完整耗时"]) {
    await card.getByRole("combobox", { name: "耗时口径", exact: true }).selectOption({ label });
    await expect(card.getByText(`${label} · 平均`, { exact: true })).toBeVisible();
  }
  await panel.getByRole("button", { name: "token", exact: true }).click();
  card = currentCard("token");
  await card.getByRole("combobox", { name: "token 口径", exact: true }).selectOption({ label: "已记录输入 token" });
  await expect(card.getByText("已记录输入 token", { exact: true }).locator("..").getByText(new Intl.NumberFormat("en-US").format(input), { exact: true })).toBeVisible();
  const recordedUsage = displayedMetrics.totals.tokens.known_usage_attempts;
  const missingUsage = displayedMetrics.totals.tokens.unknown_usage_attempts;
  expect(recordedUsage + missingUsage).toBe(4);
  await definition(card, "用量记录覆盖率", percent(recordedUsage / (recordedUsage + missingUsage)));
  await definition(card, "有可用用量记录的尝试", new Intl.NumberFormat("en-US").format(recordedUsage));
  await definition(card, "无可用用量记录的尝试", new Intl.NumberFormat("en-US").format(missingUsage));
  await card.getByRole("combobox", { name: "token 口径", exact: true }).selectOption({ label: "已记录输出 token" });
  await expect(card.getByText("已记录输出 token", { exact: true }).locator("..").getByText(new Intl.NumberFormat("en-US").format(expectedTokens.completion_tokens), { exact: true })).toBeVisible();
  await capture("tokens");
  await panel.getByRole("button", { name: "缓存", exact: true }).click();
  card = currentCard("缓存");
  await definition(card, "已记录缓存读取", new Intl.NumberFormat("en-US").format(hitTokens));
  await definition(card, "输入 token", new Intl.NumberFormat("en-US").format(input));
  await card.getByRole("combobox", { name: "缓存口径", exact: true }).selectOption({ label: "响应缓存命中率" });
  await expect(card.getByText("响应缓存命中率", { exact: true }).filter({ visible: true }).locator("..").getByText(percent(cache.hit_rate), { exact: true })).toBeVisible();
  const cacheDetail = panel.getByRole("heading", { name: "响应缓存与 token 明细", exact: true }).locator("xpath=ancestor::section[1]");
  await definition(cacheDetail, "命中请求", "1");
  await capture("cache");
  await panel.getByRole("button", { name: "费用", exact: true }).click();
  card = currentCard("费用");
  await expect(card.getByText("已计价费用", { exact: true }).locator("..").getByText(money(expectedFee), { exact: true })).toBeVisible();
  await definition(card, "最终请求", "4");
  await definition(card, "计费尝试", "4");
  await definition(card, "待计价尝试", "0");
  await definition(card, "费用调整", "¥0.00");
  await capture("cost");
  await test.info().attach("real-api-metrics-public-aggregates", { contentType: "application/json", body: Buffer.from(JSON.stringify({ metrics, memberTotals: own.totals, expectedFee: expectedFee.toString(), latencies, gatewayResponseCacheHit: true, providerTokenCachePositiveObserved: hitTokens > 0 }, null, 2)) });
});
