import { expect, test, type Response } from "@playwright/test";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
const baseUrl: string = process.env.TENANT_OVERVIEW_URL ?? state.tenantUrl;
const base = new URL(baseUrl);
if (state.ready !== true || state.realBackend !== true || state.mockUpstream !== false) throw new Error("Real isolated backend is required");
if (!["http:", "https:"].includes(base.protocol) || !["localhost", "127.0.0.1", "[::1]"].includes(base.hostname) || base.username || base.password) throw new Error("Only an isolated loopback overview URL is allowed");
if (typeof auth.metricsOwner?.email !== "string" || typeof auth.metricsOwner?.password !== "string") throw new Error("Existing isolated owner credentials are required");

test("用量汇总解释未授权或未知模型，保留这组的调用次数", async ({ context, page }) => {
  const login = await context.request.post(new URL("/api/login", base).href, { data: auth.metricsOwner });
  expect(login.status()).toBe(200);
  try {
    await page.goto(`${baseUrl.replace(/\/$/, "")}/console`);
    const recent = await page.evaluate(async () => {
      const response = await fetch("/api/usage/calls?limit=1");
      if (!response.ok) throw new Error("无法读取隔离环境中的历史调用");
      return (await response.json()).items[0]?.occurred_at;
    });
    expect(typeof recent).toBe("string");
    const last = Date.parse(recent);
    const query = new URLSearchParams({ group: "day", from: new Date(last - 7 * 86_400_000).toISOString(), to: new Date(last + 86_400_000).toISOString() });
    const summary = page.waitForResponse(r => {
      const url = new URL(r.url());
      return url.pathname === "/api/usage/summary" && url.searchParams.get("from") === query.get("from");
    });
    await page.goto(new URL("#usage?" + query, base).href);
    const response = await summary;
    expect(response.status()).toBe(200);
    const rows: { period_start: string; model: { id: string | null; name: string }; requests: number }[] = (await response.json()).items;
    const unknown = rows.filter(row => row.model.id === null && row.model.name === "");
    expect(unknown.length).toBeGreaterThan(0);
    const table = page.getByRole("table", { name: "用量汇总", exact: true });
    const displayed = table.getByRole("row").filter({ has: page.getByRole("cell", { name: "未授权或未知模型", exact: true }) });
    await expect(displayed).toHaveCount(unknown.length);
    const calendar = new Intl.DateTimeFormat("sv-SE", { timeZone: "Asia/Shanghai" });
    for (const group of unknown) {
      const day = calendar.format(new Date(group.period_start));
      const row = displayed.filter({ has: page.getByRole("cell", { name: day, exact: true }) });
      await expect(row.locator('td[data-label="调用次数"]')).toHaveText(String(group.requests));
    }
    await expect(page.getByText(/调用记录没有可识别的模型，或模型已不在组织当前授权范围内/)).toBeVisible();
    const ordinary = rows.find(row => row.model.id && row.model.name);
    expect(ordinary).toBeDefined();
    await page.getByRole("searchbox", { name: "搜索汇总模型" }).fill(ordinary!.model.name);
    await expect(displayed).toHaveCount(0);
    await expect(page.getByText(/调用记录没有可识别的模型，或模型已不在组织当前授权范围内/)).toHaveCount(0);
  } finally {
    expect((await context.request.post(new URL("/api/logout", base).href, { data: {} })).status()).toBe(204);
  }
});

test("已认证概览不等组织资料返回才开始读取统计", async ({ context, page }, info) => {
  const errors: string[] = [];
  const writes: string[] = [];
  page.on("pageerror", () => errors.push("pageerror"));
  page.on("console", message => { if (message.type() === "error") errors.push("console error"); });
  page.on("request", request => {
    const url = new URL(request.url());
    if (url.origin === base.origin && url.pathname.startsWith("/api/") && request.method() !== "GET") writes.push(`${request.method()} ${url.pathname}`);
  });
  const login = await context.request.post(new URL("api/login", base).href, { data: auth.metricsOwner });
  expect(login.status(), "reuse the retained local owner without seeding or inference").toBe(200);
  try {
    const paths = ["/api/session", "/api/org", "/api/usage/metrics", "/api/usage/summary", "/api/budgets", "/api/billing/balance"];
    const answers = paths.map(path => page.waitForResponse(response => new URL(response.url()).pathname === path && response.request().method() === "GET"));
    await page.goto(new URL("#org", base).href, { waitUntil: "domcontentloaded" });
    const responses = await Promise.all(answers);
    const byPath = new Map<string, Response>();
    for (const response of responses) {
      expect(response.status(), new URL(response.url()).pathname).toBe(200);
      await response.finished();
      byPath.set(new URL(response.url()).pathname, response);
    }
    const bootstrap = await byPath.get("/api/session")!.json();
    expect(bootstrap).toMatchObject({ authed: true, tenant: { status: "active" }, member: { role: "owner" } });
    const timings = Object.fromEntries([...byPath].map(([path, response]) => [path, response.request().timing()]));
    await info.attach("overview-request-timings", { body: JSON.stringify(timings, null, 2), contentType: "application/json" });
    const org = timings["/api/org"]!;
    expect(org.requestStart, "organization profile request was actually sent").toBeGreaterThanOrEqual(0);
    expect(org.responseEnd, "organization profile response actually completed").toBeGreaterThanOrEqual(org.requestStart);
    const orgCompletedAt = org.startTime + org.responseEnd;
    for (const path of ["/api/usage/metrics", "/api/usage/summary", "/api/budgets", "/api/billing/balance"]) {
      const timing = timings[path]!;
      expect(timing.requestStart, `${path} was actually sent`).toBeGreaterThanOrEqual(0);
      expect(timing.startTime, `${path} must start before the independent organization profile finishes`).toBeLessThan(orgCompletedAt);
    }
    await expect(page).toHaveTitle("Routio 组织门户");
    await expect(page.getByRole("heading", { name: "概览", exact: true })).toBeVisible();
    await expect(page.getByRole("group", { name: "API 指标概览", exact: true })).toHaveAttribute("aria-busy", "false");
    await expect(page.getByRole("alert")).toHaveCount(0);
    expect(errors).toEqual([]);
    expect(writes, "cold overview performs reads only").toEqual([]);
    await page.screenshot({ path: info.outputPath("overview-1440.png"), fullPage: false });
  } finally {
    expect((await context.request.post(new URL("api/logout", base).href, { data: {} })).status()).toBe(204);
  }
});

for (const portal of ["tenant", "admin"] as const) {
  test(`${portal} 概览按摘要读取，进入分析后加载同一窗口的完整指标`, async ({ context, page }, info) => {
    const origin = new URL(portal === "tenant" ? baseUrl : state.adminUrl);
    if (!["http:", "https:"].includes(origin.protocol) || !["localhost", "127.0.0.1", "[::1]"].includes(origin.hostname) || origin.username || origin.password) throw new Error("需要本地隔离门户");
    const prefix = portal === "tenant" ? "/api" : "/admin";
    const metricPath = prefix + "/usage/metrics";
    const account = portal === "tenant" ? auth.metricsOwner : auth.operator;
    const login = await context.request.post(new URL(prefix + "/login", origin).href, { data: account });
    expect(login.status()).toBe(200);
    const metricRequests: string[] = [];
    page.on("request", request => {
      if (new URL(request.url()).pathname === metricPath) metricRequests.push(request.url());
    });
    const metrics = () => page.waitForResponse(r => new URL(r.url()).pathname === metricPath && r.request().method() === "GET");
    try {
      const landing = metrics();
      await page.goto(new URL(portal === "tenant" ? "#org" : "#overview", origin).href);
      const compact = await landing;
      expect(compact.status()).toBe(200);
      expect(new URL(compact.url()).searchParams.get("view")).toBe("overview");
      const summary = await compact.json();
      const overview = page.getByRole("group", { name: "API 指标概览" });
      await expect(overview).toHaveAttribute("aria-busy", "false");
      await expect(overview.locator('[data-metric-kpi="requests"]').getByText(summary.totals.requests.toLocaleString("en-US"), { exact: true })).toBeVisible();
      expect(metricRequests).toHaveLength(1);
      const overviewRequests = metricRequests.length;
      const next = metrics();
      await page.getByRole("link", { name: /^查看用量分析/ }).click();
      const analysis = await next;
      expect(analysis.status()).toBe(200);
      expect(new URL(analysis.url()).searchParams.has("view")).toBe(false);
      await expect(page.getByRole("region", { name: "完整 API 指标" })).toHaveAttribute("aria-busy", "false");
      expect(metricRequests).toHaveLength(2);

      const full = await analysis.json();
      expect([summary.from, summary.to, summary.bucket]).toEqual([full.from, full.to, full.bucket]);
      expect(summary.models).toEqual([]);
      expect(full.totals).toHaveProperty("tokens");
      expect(summary.totals).not.toHaveProperty("tokens");
      const counts = ["requests", "succeeded_requests", "failed_requests", "unknown_outcome_requests", "unknown_final_attempts", "billing_attempts", "known_outcome_requests", "unpriced_attempts", "success_rate", "charged_micro_cny"];
      for (const field of counts) {
        expect(full.totals).toHaveProperty(field);
        expect(summary.totals).toHaveProperty(field, full.totals[field]);
      }
      const { p50, sample_count, missing_count } = full.totals.first_token_latency_ms;
      expect(summary.totals.first_token_latency_ms).toStrictEqual({ p50, sample_count, missing_count });
      expect(full.series.length).toBeGreaterThan(0);
      expect(summary.series).toStrictEqual(full.series.map((row: { period_start: string; requests: number; first_token_latency_ms: { avg: number | null } }) => ({
        period_start: row.period_start, requests: row.requests, first_token_latency_ms: { avg: row.first_token_latency_ms.avg },
      })));
      await info.attach("overview-comparison", {
        body: JSON.stringify({ portal, overviewRequests, analysisRequests: metricRequests.length - overviewRequests, totalRequests: full.totals.requests, periods: full.series.length, p50, sample_count, missing_count }),
        contentType: "application/json",
      });
    } finally {
      expect((await context.request.post(new URL(prefix + "/logout", origin).href, { data: {} })).status()).toBe(204);
    }
  });
}
