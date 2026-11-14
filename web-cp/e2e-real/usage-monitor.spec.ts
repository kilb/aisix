import { expect, test, type Locator, type Page, type Response } from "@playwright/test";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

// 只读真实隔离服务；不拦截响应、不造模型流量、不访问生产地址。
const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
if (state.ready !== true || state.mockUpstream !== false) throw new Error("A ready real local stack is required");
for (const url of [state.adminUrl, state.tenantUrl]) {
  if (!["127.0.0.1", "localhost"].includes(new URL(url).hostname)) throw new Error("Only isolated loopback URLs are allowed");
}

const writes = new WeakMap<Page, string[]>();
const runtimeErrors = new WeakMap<Page, string[]>();
interface LatencyAnswer { sample_count: number; missing_count: number; avg: number | null; p50: number | null; p95: number | null; p99: number | null }
interface ValuesAnswer {
  attempts: number; succeeded_attempts: number; failed_attempts: number; failure_rate: number | null;
  upstream_ttft_ms: LatencyAnswer; first_token_latency_ms: LatencyAnswer; throughput_tps: LatencyAnswer;
  tokens: { input_tokens: number; cache_hit_tokens: number; cache_hit_rate: number | null; cache_measured_attempts: number; cache_missing_attempts: number; cache_measured_input_tokens: number; cache_measured_hit_tokens: number };
}
interface ModelAnswer extends ValuesAnswer { model: { id: string; name: string }; provider_key: { id: string | null; name: string } }
interface DirectModelAnswer { models: ModelAnswer[]; totals: ValuesAnswer }

// 预期由可见展示合同及真实响应计算，不导入产品的解码、格式化或选择器。
const numberText = (value: number) => new Intl.NumberFormat("en-US", { maximumFractionDigits: 2 }).format(value);
const countText = (value: number) => new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 }).format(value);
const ratioText = (value: number | null) => value === null ? "—" : new Intl.NumberFormat("en-US", { style: "percent", maximumFractionDigits: 1 }).format(value);
const durationText = (value: number | null) => value === null ? "—" : value < 1000 ? `${numberText(value)} ms` : `${numberText(value / 1000)} s`;
const tpsText = (value: number | null) => value === null ? "—" : `${numberText(value)} token/s`;
const modelField = (card: Locator, label: string) => card.locator("dl > div").filter({ has: card.page().getByText(label, { exact: true }) });

async function assertModelValues(page: Page, model: ModelAnswer) {
  let card = page.locator("article").filter({ has: page.getByText(model.model.id, { exact: true }) });
  if (model.provider_key.id !== null) card = card.filter({ has: page.getByText(model.provider_key.id, { exact: true }) });
  await expect(card).toHaveCount(1);
  await expect(modelField(card, "首 token 延迟 · P50").locator("dd")).toHaveText(durationText(model.first_token_latency_ms.p50));
  await expect(modelField(card, "首 token 延迟 · P50")).toContainText(`上游首帧 ${durationText(model.upstream_ttft_ms.p50)}`);
  await expect(modelField(card, "缓存 token 命中率").locator("dd")).toHaveText(ratioText(model.tokens.cache_hit_rate));
  await expect(modelField(card, "缓存 token 命中率")).toContainText(`命中 ${countText(model.tokens.cache_measured_hit_tokens)} / 已采样输入 ${countText(model.tokens.cache_measured_input_tokens)}`);
  await expect(modelField(card, "生成吞吐 · TPS").locator("dd")).toHaveText(tpsText(model.throughput_tps.avg));
  const details = card.locator("details");
  await details.locator("summary").click();
  await expect(modelField(card, "上游首帧延迟")).toContainText(`P50 ${durationText(model.upstream_ttft_ms.p50)} · P95 ${durationText(model.upstream_ttft_ms.p95)} · P99 ${durationText(model.upstream_ttft_ms.p99)}`);
  await expect(modelField(card, "上游首帧延迟")).toContainText(`实测 ${countText(model.upstream_ttft_ms.sample_count)} · 缺失 ${countText(model.upstream_ttft_ms.missing_count)}`);
  await expect(modelField(card, "客户端首 token 延迟")).toContainText(`P50 ${durationText(model.first_token_latency_ms.p50)} · P95 ${durationText(model.first_token_latency_ms.p95)}`);
  await expect(modelField(card, "客户端首 token 延迟")).toContainText(`实测 ${countText(model.first_token_latency_ms.sample_count)} · 缺失 ${countText(model.first_token_latency_ms.missing_count)}`);
  await expect(modelField(card, "缓存计数样本")).toContainText(`已报告 ${countText(model.tokens.cache_measured_attempts)} · 未报告 ${countText(model.tokens.cache_missing_attempts)} · 全部输入 ${countText(model.tokens.input_tokens)}`);
  await details.locator("summary").click();
}
test.beforeEach(async ({ page }) => {
  const unexpectedWrites: string[] = [];
  const errors: string[] = [];
  writes.set(page, unexpectedWrites); runtimeErrors.set(page, errors);
  page.on("pageerror", error => errors.push(error.message));
  page.on("request", request => {
    const url = new URL(request.url());
    if (!["GET", "HEAD"].includes(request.method()) && !/^\/(?:admin|api)\/(?:login|logout)$/.test(url.pathname)) unexpectedWrites.push(`${request.method()} ${url.pathname}`);
  });
});
test.afterEach(async ({ page }) => {
  expect(writes.get(page)).toEqual([]);
  expect(runtimeErrors.get(page)).toEqual([]);
});

async function login(page: Page, scope: "admin" | "tenant") {
  const base = scope === "admin" ? state.adminUrl : state.tenantUrl;
  const user = scope === "admin" ? auth.operator : auth.metricsOwner;
  await page.goto(base);
  await page.getByLabel("邮箱", { exact: true }).fill(user.email);
  await page.getByLabel("口令", { exact: true }).fill(user.password);
  const response = page.waitForResponse(answer => new URL(answer.url()).pathname === (scope === "admin" ? "/admin/login" : "/api/login") && answer.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await response).status()).toBe(200);
  await page.waitForLoadState("networkidle");
  return base;
}

function matching(response: Response, path: string, filters: Record<string, string>): boolean {
  const url = new URL(response.url());
  return response.request().method() === "GET" && url.pathname === path && Object.entries(filters).every(([key, value]) => url.searchParams.get(key) === value);
}

for (const scope of ["tenant", "admin"] as const) {
  test(`${scope}: success and failure filters query the complete record scope`, async ({ page }) => {
    const base = await login(page, scope);
    const path = scope === "admin" ? "/admin/pricing/charges" : "/api/usage/calls";
    await page.goto(`${base}/#calls`);
    const filters = page.getByRole("region", { name: "调用结果筛选", exact: true });
    await expect(filters).toBeVisible();
    for (const outcome of ["failed", "succeeded"]) {
      const waiting = page.waitForResponse(response => matching(response, path, { outcome }));
      await filters.getByLabel("调用结果", { exact: true }).selectOption(outcome);
      const response = await waiting;
      expect(response.status()).toBe(200);
      expect(new URL(response.url()).searchParams.has("before")).toBe(false);
      const body = await response.json();
      expect(Array.isArray(body.items)).toBe(true);
      expect(body.items.length, "Retained real data must contain each outcome").toBeGreaterThan(0);
      for (const item of body.items) {
        if (scope === "tenant") expect(item.status).toBe(outcome);
        else {
          expect(item.is_final_attempt).toBe(true);
          if (outcome === "succeeded") {
            expect(item.status_code).toBeGreaterThanOrEqual(200);
            expect(item.status_code).toBeLessThan(300);
            expect(item.error_class === null || item.error_class === "").toBe(true);
          } else expect(Boolean(item.error_class) || Number.isInteger(item.status_code) && (item.status_code < 200 || item.status_code >= 300)).toBe(true);
        }
      }
      expect(new URLSearchParams(page.url().split("?")[1]).get("outcome")).toBe(outcome);
    }
    await page.goBack();
    await expect(filters.getByLabel("调用结果", { exact: true })).toHaveValue("failed");
    await filters.getByRole("button", { name: /更多筛选/ }).click();
    await expect(filters.getByLabel("响应缓存", { exact: true })).toBeVisible();
    await expect(filters.getByLabel("实际服务档位", { exact: true })).toBeVisible();
    await expect(filters.getByLabel("API 操作", { exact: true })).toBeVisible();
    if (scope === "tenant") await expect(filters.getByLabel("HTTP 状态码", { exact: true })).toHaveCount(0);
    const waiting = page.waitForResponse(response => matching(response, path, { outcome: "failed", cache_hit: "false" }));
    await filters.getByLabel("响应缓存", { exact: true }).selectOption("false");
    expect((await waiting).status()).toBe(200);
    await expect(filters.getByLabel("调用结果", { exact: true })).toHaveValue("failed");
    await filters.getByRole("button", { name: "清除调用结果筛选", exact: true }).click();
    await expect(filters.getByLabel("调用结果", { exact: true })).toHaveValue("");
    await expect(filters.getByLabel("响应缓存", { exact: true })).toHaveValue("");
  });
}

test("legacy failed-call links retain outcome when combining and clearing filters", async ({ page }) => {
  const base = await login(page, "tenant");
  const initial = page.waitForResponse(response => matching(response, "/api/usage/calls", { outcome: "failed" }));
  await page.goto(`${base}/#calls?status=failed`);
  expect((await initial).status()).toBe(200);
  const filters = page.getByRole("region", { name: "调用结果筛选", exact: true });
  await expect(filters.getByLabel("调用结果", { exact: true })).toHaveValue("failed");
  await filters.getByRole("button", { name: /更多筛选/ }).click();
  const combined = page.waitForResponse(response => matching(response, "/api/usage/calls", { outcome: "failed", cache_hit: "false" }));
  await filters.getByLabel("响应缓存", { exact: true }).selectOption("false");
  expect((await combined).status()).toBe(200);
  await expect(filters.getByLabel("调用结果", { exact: true })).toHaveValue("failed");
  const query = new URLSearchParams(page.url().split("?")[1]);
  expect(query.get("outcome")).toBe("failed");
  expect(query.has("status")).toBe(false);
  const cleared = page.waitForResponse(response => { const url = new URL(response.url()); return url.pathname === "/api/usage/calls" && !url.searchParams.has("outcome") && !url.searchParams.has("cache_hit"); });
  await filters.getByRole("button", { name: "清除调用结果筛选", exact: true }).click();
  expect((await cleared).status()).toBe(200);
  await expect(filters.getByLabel("调用结果", { exact: true })).toHaveValue("");
  expect(new URLSearchParams(page.url().split("?")[1]).has("status")).toBe(false);
});

test("admin visible time and identity controls combine before pagination and retain precise deep links", async ({ page }) => {
  const base = await login(page, "admin");
  const first = page.waitForResponse(response => matching(response, "/admin/pricing/charges", {}));
  await page.goto(`${base}/#calls`);
  const initial = await first;
  expect(initial.status()).toBe(200);
  const body = await initial.json();
  const sample = body.items.find((item: { user_id?: string; requested_model_id?: string; occurred_at?: string }) => item.user_id && item.requested_model_id && item.occurred_at);
  expect(sample, "Retained real calls must contain a model and member for scope verification").toBeDefined();
  const at = Date.parse(sample.occurred_at);
  expect(Number.isFinite(at)).toBe(true);
  const from = new Date(at - 60_000);
  const to = new Date(at + 60_000);
  from.setUTCMilliseconds(123); to.setUTCMilliseconds(987);
  const scope = { from: from.toISOString(), to: to.toISOString(), member_id: sample.user_id, model_id: sample.requested_model_id, ...(sample.upstream_account_id ? { provider_key_id: sample.upstream_account_id } : {}) };
  const linked = page.waitForResponse(response => matching(response, "/admin/pricing/charges", scope));
  await page.goto(`${base}/#calls?${new URLSearchParams(scope)}`);
  const linkedAnswer = await linked;
  expect(linkedAnswer.status()).toBe(200);
  const linkedBody = await linkedAnswer.json();
  expect(linkedBody.items.some((item: { event_id: string }) => item.event_id === sample.event_id)).toBe(true);
  const controls = page.locator("details").filter({ has: page.locator("summary").filter({ hasText: "时间与归属筛选" }) });
  await controls.locator("summary").click();
  for (const label of ["开始时间（北京时间）", "结束时间（不含，北京时间）", "模型 ID", "成员 ID", "上游账号 ID", "请求 ID"]) await expect(controls.getByLabel(label, { exact: true })).toBeVisible();
  await controls.getByLabel("请求 ID", { exact: true }).fill("ui-scope-no-match-20261112");
  const refined = page.waitForResponse(response => matching(response, "/admin/pricing/charges", { ...scope, request_id: "ui-scope-no-match-20261112" }));
  await controls.getByRole("button", { name: "应用时间与归属筛选", exact: true }).click();
  const refinedAnswer = await refined;
  expect(refinedAnswer.status()).toBe(200);
  expect(new URL(refinedAnswer.url()).searchParams.has("before")).toBe(false);
  expect((await refinedAnswer.json()).items).toEqual([]);
  const query = new URLSearchParams(page.url().split("?")[1]);
  expect(query.get("from")).toBe(scope.from);
  expect(query.get("to")).toBe(scope.to);
  const restored = page.waitForResponse(response => matching(response, "/admin/pricing/charges", scope) && !new URL(response.url()).searchParams.has("request_id"));
  await page.goBack();
  expect((await restored).status()).toBe(200);
  await controls.locator("summary").click();
  await expect(controls.getByLabel("请求 ID", { exact: true })).toHaveValue("");
});

test("direct model monitoring reads real attempt metrics and remains usable on narrow screens", async ({ page }) => {
  const base = await login(page, "admin");
  const waiting = page.waitForResponse(response => new URL(response.url()).pathname === "/admin/usage/direct-models");
  await page.goto(`${base}/#upstream-monitoring`);
  await expect(page.getByRole("heading", { name: "上游监测", exact: true })).toBeVisible();
  const response = await waiting;
  expect(response.status()).toBe(200);
  const data = await response.json() as DirectModelAnswer;
  expect(data.models.length, "Retained local data needs actual direct models").toBeGreaterThan(0);
  expect(data.totals.attempts, "Metrics E2E requires real dispatch evidence").toBeGreaterThan(0);
  expect(data.totals.failed_attempts).toBeGreaterThan(0);
  expect(data.totals.succeeded_attempts).toBeGreaterThan(0);
  const expected = data.totals.failed_attempts / (data.totals.failed_attempts + data.totals.succeeded_attempts);
  expect(data.totals.failure_rate).toBeCloseTo(expected, 10);
  const cards = page.locator("article").filter({ has: page.getByText("上游尝试", { exact: true }) });
  const modelName = [...data.models].sort((left, right) => left.model.name.localeCompare(right.model.name))[0]!.model.name;
  await expect(cards.first().getByRole("heading", { level: 3 })).toHaveText(modelName);
  for (const label of ["失败率", "首 token 延迟 · P50", "缓存 token 命中率", "生成吞吐 · TPS"]) await expect(page.getByRole("heading", { name: label, exact: true })).toBeVisible();
  for (const [label, value] of [
    ["上游尝试", countText(data.totals.attempts)],
    ["失败率", ratioText(data.totals.failure_rate)],
    ["首 token 延迟 · P50", durationText(data.totals.first_token_latency_ms.p50)],
    ["缓存 token 命中率", ratioText(data.totals.tokens.cache_hit_rate)],
    ["生成吞吐 · TPS", tpsText(data.totals.throughput_tps.avg)],
  ]) {
    const kpi = page.locator("section").filter({ has: page.getByRole("heading", { name: label, exact: true }) });
    await expect(kpi.locator(":scope > strong")).toHaveText(value!);
  }
  const sorted = [...data.models].sort((left, right) => left.model.name.localeCompare(right.model.name));
  const cacheKpi = page.locator("section").filter({ has: page.getByRole("heading", { name: "缓存 token 命中率", exact: true }) });
  await expect(cacheKpi.locator(":scope > p")).toHaveText(`命中 ${countText(data.totals.tokens.cache_measured_hit_tokens)} / 已采样输入 ${countText(data.totals.tokens.cache_measured_input_tokens)} · 未报告 ${countText(data.totals.tokens.cache_missing_attempts)}`);
  for (const model of sorted.slice(0, 10)) await assertModelValues(page, model);
  // 缺失与零及不同计时时钟需要真实样本；没有这些数据时明确阻止验收。
  const clockSample = data.models.find(model => model.first_token_latency_ms.p50 !== null && model.upstream_ttft_ms.p50 !== null && model.first_token_latency_ms.p50 !== model.upstream_ttft_ms.p50);
  const zeroSample = data.models.find(model => model.first_token_latency_ms.sample_count > 0 && model.first_token_latency_ms.p50 === 0);
  const missingSample = data.models.find(model => model.first_token_latency_ms.sample_count === 0 && model.first_token_latency_ms.p50 === null);
  expect(clockSample, "Retained real monitoring data must distinguish first-frame and delivered-token clocks").toBeDefined();
  expect(zeroSample, "Retained real monitoring data must include a measured zero, not a fabricated response").toBeDefined();
  expect(missingSample, "Retained real monitoring data must include an unmeasured direct model").toBeDefined();
  for (const model of [clockSample!, zeroSample!, missingSample!]) {
    await page.getByRole("searchbox", { name: "搜索监测模型", exact: true }).fill(model.model.id);
    await assertModelValues(page, model);
  }
  await page.getByRole("searchbox", { name: "搜索监测模型", exact: true }).fill("");
  for (const width of [1100, 900, 390]) {
    await page.setViewportSize({ width, height: 1000 });
    await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
    await expect(cards.first()).toBeVisible();
    await expect(page.getByRole("navigation", { name: "监测模型分页", exact: true })).toBeVisible();
  }
  const extra = page.locator("details").filter({ has: page.getByText(/^模型与账号筛选/) });
  await extra.locator("summary").click();
  const idInput = extra.getByLabel("直连模型 ID", { exact: true });
  const modelId = data.models[0]!.model.id;
  await idInput.fill(modelId);
  const filtered = page.waitForResponse(answer => matching(answer, "/admin/usage/direct-models", { model_id: modelId }));
  await extra.getByRole("button", { name: "应用筛选", exact: true }).click();
  const answer = await filtered;
  expect(answer.status()).toBe(200);
  const subset = await answer.json() as DirectModelAnswer;
  expect(subset.models.length).toBeGreaterThan(0);
  expect(subset.models.every(model => model.model.id === modelId)).toBe(true);
});
