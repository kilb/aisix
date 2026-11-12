import { expect, test, type Page } from "@playwright/test";
import { readFileSync, mkdirSync } from "node:fs";
import { join, resolve } from "node:path";

// 只使用保留的真实隔离世界；没有seed、响应替换、模型调用或上游探测。
const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
if (!state.ready || state.mockUpstream !== false) throw new Error("REFINED_QA_SESSION must be a ready, real, isolated stack");
const output = resolve(process.env.REFINED_QA_OUTPUT!);
const app: string = "tenant";
const baseUrl = state[app === "tenant" ? "tenantUrl" : "adminUrl"];
if (!['127.0.0.1', 'localhost'].includes(new URL(baseUrl).hostname)) throw new Error("Only isolated loopback URLs are allowed");
const metricPath = app === "tenant" ? "/api/usage/metrics" : "/admin/usage/metrics";
const scope = "from=2026-11-12T03%3A12%3A00.000Z&to=2026-11-12T03%3A17%3A00.000Z";
const analysisHash = app === "tenant" ? "#usage" : "#analytics";
const issues = new WeakMap<Page, string[]>();
type HealthPhase = "signed-out" | "authenticated" | "logging-out";
interface Health { phase: HealthPhase; rawConsole: object[]; rawHttpErrors: object[]; expectedBootstrap401: object[]; unexpected: object[] }
const health = new WeakMap<Page, Health>();
const bootstrapPaths = new Set(["/api/me", "/api/session", "/admin/me", "/admin/session"]);
function expectedBootstrap401(page: Page, target: string, phase: HealthPhase): boolean {
  if (phase === "authenticated" || !/^https?:\/\//.test(target)) return false;
  const url = new URL(target);
  return url.origin === new URL(page.url()).origin && bootstrapPaths.has(url.pathname);
}
const requests = new WeakMap<Page, {method: string; path: string; query: string}[]>();
test.use({ trace: "off", viewport: { width: 1440, height: 1000 }, timezoneId: "America/Los_Angeles" });

async function settle(page: Page): Promise<void> {
  await page.evaluate(() => document.fonts.ready);
  await page.waitForLoadState("networkidle");
  await expect(page.getByText("载入中…", { exact: true })).toHaveCount(0);
}
async function login(page: Page, who = app === "tenant" ? auth.metricsOwner : auth.operator, target = baseUrl): Promise<void> {
  if (!["127.0.0.1", "localhost"].includes(new URL(target).hostname)) throw new Error("Only isolated login origins are allowed");
  health.get(page)!.phase = "signed-out";
  await page.goto(target);
  await page.getByLabel("邮箱", { exact: true }).fill(who.email);
  await page.getByLabel("口令", { exact: true }).fill(who.password);
  const response = page.waitForResponse(r => new URL(r.url()).pathname === (app === "tenant" ? "/api/login" : "/admin/login") && r.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await response).status()).toBe(200);
  health.get(page)!.phase = "authenticated";
  await settle(page);
}
async function goto(page: Page, hash: string): Promise<void> {
  await page.goto(baseUrl + "/" + hash);
  await settle(page);
}
// localhost 的 Secure cookie 由真实页面持有；oracle沿同源浏览器通道读取。
async function browserGet(page: Page, target: string): Promise<{ status(): number; json(): Promise<any> }> {
  const url = new URL(target, page.url());
  if (url.origin !== new URL(page.url()).origin) throw new Error("Oracle must use the current real page origin");
  const answer = await page.evaluate(async path => {
    const response = await fetch(path, { credentials: "include" });
    return { status: response.status, body: await response.json() };
  }, url.pathname + url.search);
  return { status: () => answer.status, json: async () => answer.body };
}

async function logout(page: Page): Promise<void> {
  health.get(page)!.phase = "logging-out";
  const status = await page.evaluate(async () => {
    const response = await fetch("/api/logout", { method: "POST", headers: { "content-type": "application/json" }, body: "{}" });
    return response.status;
  });
  expect(status, "real local logout status").toBe(204);
  health.get(page)!.phase = "signed-out";
  test.info().annotations.push({ type: "real-local-logout", description: "same-origin POST204 with JSON content-type" });
}

async function metricOracle(page: Page): Promise<any> {
  const path = metricPath + "?" + scope + "&bucket=minute" + (app === "admin" ? "&tenant_id=b7928a9a-ba93-4209-92a4-c7fd806ffd9d" : "");
  const response = await browserGet(page, baseUrl + path);
  expect(response.status()).toBe(200);
  const data = await response.json();
  for (const [key, value] of Object.entries({ requests: 4, succeeded_requests: 3, failed_requests: 1, unknown_outcome_requests: 0, success_rate: 0.75, charged_micro_cny: "941181" })) {
    expect(Object.hasOwn(data.totals, key), `explicit ${key}`).toBe(true);
    expect(data.totals[key], key).toBe(value);
  }
  expect(data.totals.first_token_latency_ms.sample_count).toBe(2);
  expect(data.totals.first_token_latency_ms.missing_count).toBe(2);
  expect(data.totals.first_token_latency_ms.p99).toBeCloseTo(8362.43, 7);
  return data;
}
test.beforeEach(async ({ page }) => {
  const errors: string[] = [];
  const ledger: {method: string; path: string; query: string}[] = [];
  issues.set(page, errors); requests.set(page, ledger);
  const telemetry: Health = { phase: "signed-out", rawConsole: [], rawHttpErrors: [], expectedBootstrap401: [], unexpected: [] };
  health.set(page, telemetry);
  page.on("pageerror", e => { errors.push(e.message); telemetry.unexpected.push({ kind: "pageerror", phase: telemetry.phase, message: e.message }); });
  page.on("console", message => {
    const event = { kind: "console", type: message.type(), text: message.text(), location: message.location(), phase: telemetry.phase };
    telemetry.rawConsole.push(event);
    if (message.type() !== "error") return;
    if (/401|Unauthorized/.test(message.text()) && expectedBootstrap401(page, message.location().url, telemetry.phase)) telemetry.expectedBootstrap401.push(event);
    else { telemetry.unexpected.push(event); errors.push(message.text()); }
  });
  page.on("response", response => {
    const url = new URL(response.url());
    if (response.status() < 400 || !/^\/(?:api|admin)\//.test(url.pathname)) return;
    const event = { kind: "http", url: response.url(), status: response.status(), phase: telemetry.phase };
    telemetry.rawHttpErrors.push(event);
    if (response.status() === 401 && expectedBootstrap401(page, response.url(), telemetry.phase)) telemetry.expectedBootstrap401.push(event);
    else { telemetry.unexpected.push(event); errors.push(`HTTP ${response.status()} ${url.pathname}`); }
  });
  page.on("request", r => { const url = new URL(r.url()); if (/^\/(?:api|admin)\//.test(url.pathname)) ledger.push({ method: r.method(), path: url.pathname, query: url.search }); });
});
test.afterEach(async ({ page }, info) => {
  mkdirSync(join(output, "screenshots"), { recursive: true });
  await page.screenshot({ path: join(output, "screenshots", app + "-" + info.title.replace(/[^a-zA-Z0-9]+/g, "-") + ".png"), fullPage: false });
  await logout(page);
  await test.info().attach("raw-console-and-auth-phase-health", { body: JSON.stringify(health.get(page)), contentType: "application/json" });
  expect.soft(health.get(page)!.unexpected, "unexpected console/HTTP errors").toEqual([]);
  expect.soft(issues.get(page) ?? [], "runtime / CSP console errors").toEqual([]);
  expect.soft((requests.get(page) ?? []).filter(r => r.method !== "GET" && r.method !== "HEAD" && !/^\/(?:api|admin)\/(?:login|logout)$/.test(r.path)), "no automatic business/probe/model writes").toEqual([]);

});

test("overview-fixed-four-kpi-and-explicit-analysis", async ({ page }) => {
  await login(page);
  await goto(page, app === "tenant" ? "#org" : "#overview");
  for (const name of ["请求", "延迟", "token", "缓存", "费用"]) await expect(page.getByRole("button", { name, exact: true })).toHaveCount(0);
  await expect(page.getByRole("link", { name: /^查看用量分析(?:\s*→)?$/ })).toBeVisible();
  await expect(page.getByRole("link", { name: "用量分析", exact: true })).toBeVisible();
  await expect(page.getByRole("link", { name: "调用记录", exact: true })).toBeVisible();
  await metricOracle(page);
  const overview = page.getByRole("group", { name: "API 指标概览", exact: true });
  await expect(overview).toBeVisible();
  await expect(overview.locator("[data-metric-kpi]")).toHaveCount(4);
  for (const label of ["请求数", "成功率", "首 token 延迟 P50", "已计价费用"]) await expect(overview.getByText(label, { exact: true })).toBeVisible();
  if (app === "tenant") {
    await expect(overview).toContainText(/请求数\s*4/);
    await expect(overview).toContainText(/首 token 延迟 P50\s*5\.59 s/);
    await expect(overview).toContainText(/实测样本 2.*无实测值 2/s);
    await expect(overview).toContainText(/已知结果样本（分母）\s*4/);

    await expect(overview.getByText("75%", { exact: true })).toBeVisible();
    await expect(overview.getByText("¥0.941181", { exact: true })).toBeVisible();
    await expect(page.getByText(/首 token.*P50/i)).toBeVisible();
  }
  const analysisLink = page.getByRole("link", { name: /^查看用量分析(?:\s*→)?$/ });
  const target = await analysisLink.getAttribute("href"); expect(target).toBeTruthy();
  await analysisLink.click(); await settle(page);
  expect(page.url().split("#")[1]).toBe(target!.split("#")[1]);
  await expect(page.getByRole("heading", { name: "用量分析", exact: true })).toBeVisible();
});

test("analysis-tabs-share-scope-and-browser-back", async ({ page }) => {
  await login(page);
  const tenant = app === "admin" ? "&tenant_id=b7928a9a-ba93-4209-92a4-c7fd806ffd9d" : "";
  const analysisCursor = (requests.get(page) ?? []).length;
  await goto(page, analysisHash + "?view=requests&" + scope + tenant);
  await expect(page.getByRole("heading", { name: "用量分析", exact: true })).toBeVisible();
  const tabs = page.getByRole("tablist").filter({ has: page.getByRole("tab", { name: "请求", exact: true }) });
  await expect(tabs.getByRole("tab")).toHaveCount(5);
  const panel = page.getByRole("tabpanel"); await expect(panel).toHaveCount(1);
  const originalNode = await panel.elementHandle();
  const stableScope = new URLSearchParams(page.url().split("?")[1]);
  expect(stableScope.has("from")).toBe(true); expect(stableScope.has("to")).toBe(true);
  const phaseMetrics = () => (requests.get(page) ?? []).slice(analysisCursor).filter(r => r.method === "GET" && r.path === metricPath);
  const activeScopeMetrics = () => phaseMetrics().filter(r => {
    const query = new URLSearchParams(r.query);
    return query.has("from") && query.has("to")
      && Date.parse(query.get("from")!) === Date.parse(stableScope.get("from")!)
      && Date.parse(query.get("to")!) === Date.parse(stableScope.get("to")!)
      && query.get("tenant_id") === stableScope.get("tenant_id");
  });
  await expect.poll(() => activeScopeMetrics().length).toBeGreaterThanOrEqual(1);
  await settle(page);
  const before = phaseMetrics().length;
  await test.info().attach("positive-initial-analysis-query", { body: JSON.stringify({ cursor: analysisCursor, activeScope: Object.fromEntries(stableScope), requests: activeScopeMetrics() }), contentType: "application/json" });
  await tabs.getByRole("tab", { name: "延迟", exact: true }).click(); await settle(page);
  await expect(tabs.getByRole("tab", { name: "延迟", exact: true })).toHaveAttribute("aria-selected", "true");
  expect(new URLSearchParams(page.url().split("?")[1]).get("view")).toBe("latency");
  for (const key of ["from", "to", "tenant_id"]) expect(new URLSearchParams(page.url().split("?")[1]).get(key)).toBe(stableScope.get(key));
  expect(await panel.evaluate((node, original) => node === original, originalNode)).toBe(true);
  expect(phaseMetrics().length).toBe(before);
  const share = page.url(); await page.reload(); await settle(page);
  await expect(tabs.getByRole("tab", { name: "延迟", exact: true })).toHaveAttribute("aria-selected", "true");
  expect(page.url()).toBe(share);
  await tabs.getByRole("tab", { name: "费用", exact: true }).click(); await settle(page);
  await page.goBack(); await settle(page);
  await expect(tabs.getByRole("tab", { name: "延迟", exact: true })).toHaveAttribute("aria-selected", "true");
  await metricOracle(page);
});

test("analysis-tabs-keyboard-and-calls-scope", async ({ page }) => {
  await login(page); await goto(page, analysisHash + "?view=requests&" + scope);
  const tabs = page.getByRole("tablist").filter({ has: page.getByRole("tab", { name: "请求", exact: true }) });
  await tabs.getByRole("tab", { name: "请求", exact: true }).focus(); await page.keyboard.press("ArrowRight");
  await expect(tabs.getByRole("tab", { name: "延迟", exact: true })).toBeFocused();
  await expect(tabs.getByRole("tab", { name: "延迟", exact: true })).toHaveAttribute("aria-selected", "true");
  await page.keyboard.press("End"); await expect(tabs.getByRole("tab", { name: "费用", exact: true })).toBeFocused();
  await expect(tabs.getByRole("tab", { name: "费用", exact: true })).toHaveAttribute("aria-selected", "true");
  await page.keyboard.press("Home"); await expect(tabs.getByRole("tab", { name: "请求", exact: true })).toBeFocused();
  await page.getByRole("link", { name: "查看调用记录", exact: true }).first().click(); await settle(page);
  expect(page.url().split("#")[1]?.split("?")[0]).toBe("calls");
  await expect(page.getByRole("heading", { name: /调用.*记录/ }).first()).toBeVisible();
  await expect(page.getByRole("tabpanel")).toHaveCount(0);
});


test("legacy-usage-prices-and-overview-remain-reachable", async ({ page }) => {
  await login(page); await goto(page, "#org");
  await expect(page.getByRole("link", { name: "概览", exact: true })).toHaveAttribute("aria-current", "page");
  await goto(page, "#usage?view=latency&" + scope);
  await expect(page.getByRole("heading", { name: "用量分析", exact: true })).toBeVisible();
  await expect(page.getByRole("tab", { name: "延迟", exact: true })).toHaveAttribute("aria-selected", "true");
  await goto(page, "#usage?page=calls");
  await expect(page.getByRole("heading", { name: /调用.*记录/ }).first()).toBeVisible();
  const fixture = JSON.parse(readFileSync(join(session, "fixture-public.json"), "utf8"));
  await logout(page);
  await page.reload();
  await expect(page.getByLabel("邮箱", { exact: true })).toBeVisible();
  const priceActor = JSON.parse(readFileSync(join(session, "fixture-private.json"), "utf8")).owner;
  test.info().annotations.push({ type: "price-fixture-actor", description: "retained fixture-private.json owner with existing A/B grants" });
  await login(page, priceActor);
  const groups = fixture.groups.map((g: any) => typeof g === "string" ? g : g.id);
  expect(groups.length).toBeGreaterThanOrEqual(2);
  await goto(page, "#prices?group=" + encodeURIComponent(groups[0]));
  await expect(page.getByRole("combobox", { name: /分组/ }).first()).toHaveValue(groups[0]);
  await goto(page, "#prices?group=" + encodeURIComponent(groups[1]));
  await expect(page.getByRole("combobox", { name: /分组/ }).first()).toHaveValue(groups[1]);
  expect((requests.get(page) ?? []).filter(r => r.path.startsWith("/admin/"))).toEqual([]);
});

test("call-record-pagination-preserves-real-four-call-total", async ({ page }) => {
  await login(page); await goto(page, "#calls");
  await expect(page.getByRole("heading", { name: /调用.*记录/ }).first()).toBeVisible();
  const before = await metricOracle(page);
  const size = page.getByRole("combobox", { name: /每页/ });
  await expect(size).toHaveCount(1);
  for (const value of ["10", "25", "50"]) {
    await size.selectOption(value); await settle(page);
    await expect(page.getByRole("button", { name: "下一页", exact: true })).toBeDisabled();
    expect((await metricOracle(page)).totals).toEqual(before.totals);
  }
});


test("analysis-preserves-exact-fee-latency-tokens-and-cache-denominators", async ({ page }) => {
  await login(page);
  const tenant = app === "admin" ? "&tenant_id=b7928a9a-ba93-4209-92a4-c7fd806ffd9d" : "";
  await goto(page, analysisHash + "?view=latency&" + scope + tenant);
  const panel = page.getByRole("tabpanel");
  const tabs = page.getByRole("tablist").filter({ has: page.getByRole("tab", { name: "请求", exact: true }) });
  const cardFor = (name: string) => panel.getByRole("heading", { name: new RegExp("^API 指标 · " + name + "$", "i") }).locator("xpath=ancestor::section[1]");
  let card = cardFor("延迟");
  const definition = async (label: string, value: string): Promise<void> => {
    const term = card.locator("dt").filter({ hasText: new RegExp("^" + label + "$") });
    await expect(term).toHaveCount(1);
    await expect(term.locator("xpath=following-sibling::dd[1]")).toHaveText(value);
  };
  await panel.getByRole("combobox", { name: "耗时口径", exact: true }).selectOption({ label: "首 token 延迟" });
  await definition("P50", "5.59 s"); await definition("P99", "8.36 s");
  await definition("实测样本", "2"); await definition("无实测值", "2");
  await tabs.getByRole("tab", { name: /^token$/i }).click(); await settle(page); card = cardFor("token");
  await panel.getByRole("combobox", { name: "token 口径", exact: true }).selectOption({ label: "已记录输入 token" });
  await expect(panel.getByText("13,208", { exact: true }).first()).toBeVisible();
  await definition("用量记录覆盖率", "100%");
  await definition("有可用用量记录的尝试", "4"); await definition("无可用用量记录的尝试", "0");
  await panel.getByRole("combobox", { name: "token 口径", exact: true }).selectOption({ label: "已记录输出 token" });
  await expect(panel.getByText("16", { exact: true }).first()).toBeVisible();
  await tabs.getByRole("tab", { name: "缓存", exact: true }).click(); await settle(page); card = cardFor("缓存");
  await definition("已记录缓存读取", "12,672"); await definition("输入 token", "13,208");
  await panel.getByRole("combobox", { name: "缓存口径", exact: true }).selectOption({ label: "响应缓存命中率" });
  await expect(panel.getByText("50%", { exact: true }).first()).toBeVisible();
  await expect(panel).toContainText(/命中.*未命中/s);
  await tabs.getByRole("tab", { name: "费用", exact: true }).click(); await settle(page); card = cardFor("费用");
  await expect(panel.getByText("¥0.941181", { exact: true }).first()).toBeVisible();
  await definition("最终请求", "4"); await definition("计费尝试", "4");
  const data = await metricOracle(page);
  expect(data.totals.tokens.cache_hit_rate).toBeCloseTo(12672 / 13208, 10);
  expect(data.totals.response_cache.hit_rate).toBe(1 / (1 + 1));
});

test("invalid-same-page-dates-hide-old-data-and-never-fetch-wide-scope", async ({ page }) => {
  await login(page);
  for (const route of ["usage", "calls"]) {
    await goto(page, "#" + route);
    const table = page.getByRole("table", { name: route === "usage" ? "用量汇总" : "调用记录", exact: true });
    if (route === "calls") await expect(table.locator("tbody tr")).toHaveCount(4);
    else await expect.poll(() => table.locator("tbody tr").count()).toBeGreaterThan(0);
    const originalRows = await table.locator("tbody tr").allTextContents();
    expect(originalRows.length).toBeGreaterThan(0);
    for (const bad of ["bad", "2026-02-31"]) {
      const before = (requests.get(page) ?? []).length;
      await page.evaluate(hash => { location.hash = hash; }, "#" + route + "?from=" + bad + "&to=2026-03-05");
      await expect(page.getByText(/(?:日期|时间|范围).*(?:无效|错误|不能|非法)|(?:无效|非法).*(?:日期|时间|范围)/).first()).toBeVisible();
      await page.waitForTimeout(120);
      const remaining = await table.locator("tbody tr").allTextContents();
      expect(remaining.filter(row => originalRows.includes(row))).toEqual([]);
      await expect(page.getByText("¥0.941181", { exact: true })).toHaveCount(0);
      const next = page.getByRole("button", { name: "下一页", exact: true });
      if (await next.count()) await expect(next).toBeDisabled();
      const accidentalWide = (requests.get(page) ?? []).slice(before).filter(r => r.method === "GET" && /^\/api\/usage(?:\/(?:summary|calls|metrics))?$/.test(r.path) && !new URLSearchParams(r.query).has("from"));
      expect(accidentalWide).toEqual([]);
      await page.evaluate(hash => { location.hash = hash; }, "#" + route);
      await settle(page);
      await expect(table.locator("tbody tr").first()).toBeVisible();
    }
  }
});

test("key-change-never-commits-old-rows-and-local-query-hides-immediately", async ({ page }) => {
  await login(page);
  const fixture = JSON.parse(readFileSync(join(session, "scope-fixture.json"), "utf8"));
  expect(fixture.emptyKeyId).toBeTruthy();
  await goto(page, "#calls?api_key_id=" + fixture.usedKeyIds[0]);
  const table = page.getByRole("table", { name: "调用记录", exact: true });
  await expect(table.locator("tbody tr")).toHaveCount(2);
  const oldRows = await table.locator("tbody tr").allTextContents();
  expect(oldRows.length).toBe(2);
  await page.evaluate(({ empty, old }) => {
    const w = window as unknown as { refinedSamples: { key: string; staleRows: string[]; query: string }[]; refinedObserver: MutationObserver; refinedRaf: number };
    w.refinedSamples = [];
    const sample = (): void => {
      const select = [...document.querySelectorAll("select")].find(n => n.value === empty);
      const rows = [...document.querySelectorAll("main tbody tr")].map(n => n.textContent ?? "");
      w.refinedSamples.push({ key: select?.value ?? "", staleRows: rows.filter(text => old.some(o => o.replace(/\s+/g, "").trim() === text.replace(/\s+/g, "").trim())), query: location.hash });
    };
    w.refinedObserver = new MutationObserver(sample); w.refinedObserver.observe(document.querySelector("main")!, { subtree: true, childList: true, characterData: true, attributes: true });
    const frame = (): void => { sample(); w.refinedRaf = requestAnimationFrame(frame); }; frame();
    location.hash = "#calls?api_key_id=" + empty;
  }, { empty: fixture.emptyKeyId, old: oldRows });
  await expect(page.getByRole("combobox", { name: "密钥", exact: true })).toHaveValue(fixture.emptyKeyId);
  await settle(page);
  const samples = await page.evaluate(() => { const w = window as any; w.refinedObserver.disconnect(); cancelAnimationFrame(w.refinedRaf); return w.refinedSamples; });
  expect(samples.length).toBeGreaterThan(0);
  expect(samples.filter((x: any) => x.key === fixture.emptyKeyId && x.staleRows.length > 0)).toEqual([]);
  await test.info().attach("real-key-scope-dom-commits", { body: JSON.stringify(samples), contentType: "application/json" });
  await goto(page, "#calls");
  await expect(table.locator("tbody tr")).toHaveCount(4);
  const allRows = await table.locator("tbody tr").allTextContents(); expect(allRows.length).toBe(4);
  const before = await metricOracle(page);
  const search = page.getByRole("searchbox", { name: "搜索已加载调用", exact: true });
  await page.evaluate(old => {
    const w = window as any; w.refinedQuerySamples = [];
    const sample = (kind: string): void => {
      const search = document.querySelector('input[type="search"]') as HTMLInputElement | null;
      const rows = [...document.querySelectorAll("main tbody tr")].map(n => n.textContent ?? "");
      w.refinedQuerySamples.push({ kind, at: performance.now(), q: search?.value ?? "", hash: location.hash, rowCount: rows.length, staleRows: rows.filter(text => old.some((o: string) => o.replace(/\s+/g, "").trim() === text.replace(/\s+/g, "").trim())) });
    };
    w.refinedQuerySample = sample;
    w.refinedQueryObserver = new MutationObserver(() => sample("mutation")); w.refinedQueryObserver.observe(document.querySelector("main"), { subtree: true, childList: true, characterData: true, attributes: true });
    document.querySelector('input[type="search"]')?.addEventListener("input", () => { sample("native-input-sync"); queueMicrotask(() => sample("native-input-microtask")); }, { once: true });
    const frame = (): void => { sample("raf"); w.refinedQueryRaf = requestAnimationFrame(frame); }; frame();
  }, allRows);
  await search.fill("refined-query-guaranteed-no-match-1112");
  await page.evaluate(() => { const w = window as any; w.refinedQuerySample("after-fill"); });
  await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))));
  const querySamples = await page.evaluate(() => { const w = window as any; w.refinedQueryObserver.disconnect(); cancelAnimationFrame(w.refinedQueryRaf); return w.refinedQuerySamples; });
  await test.info().attach("real-local-query-dom-commits", { body: JSON.stringify(querySamples), contentType: "application/json" });
  const rendered = querySamples.filter((x: any) => ["mutation", "raf", "after-fill"].includes(x.kind));
  const newQuery = "refined-query-guaranteed-no-match-1112";
  expect(rendered.filter((x: any) => x.kind === "mutation" && x.q === newQuery).length).toBeGreaterThanOrEqual(1);
  expect(rendered.filter((x: any) => x.kind === "raf" && x.q === newQuery).length).toBeGreaterThanOrEqual(2);
  expect(rendered.filter((x: any) => x.q === newQuery && x.staleRows.length > 0)).toEqual([]);
  for (const sample of rendered.filter((x: any) => x.q === newQuery)) expect(sample.rowCount).toBe(0);
  await expect(search).toHaveValue(newQuery);
  const immediateRows = await table.locator("tbody tr").allTextContents();
  expect(immediateRows).toEqual([]);
  expect(new URLSearchParams(page.url().split("?")[1]).get("q")).toBe("refined-query-guaranteed-no-match-1112");
  expect((await metricOracle(page)).totals).toEqual(before.totals);
});

test("unknown-url-key-and-model-stay-visible-and-never-become-all", async ({ page }) => {
  await login(page);
  const key = "11111111-1111-4111-8111-111111111111";
  const model = "22222222-2222-4222-8222-222222222222";
  const phaseStart = (requests.get(page) ?? []).length;
  for (const route of ["usage", "calls"]) {
    await goto(page, `#${route}?api_key_id=${key}&model_id=${model}`);
    const keySelect = page.getByRole("combobox", { name: "密钥", exact: true });
    const modelSelect = page.getByRole("combobox", { name: "模型", exact: true });
    await expect(keySelect).toHaveValue(key); await expect(modelSelect).toHaveValue(model);
    await expect(keySelect.locator("option:checked")).toContainText(key);
    await expect(modelSelect.locator("option:checked")).toContainText(model);
    for (const [param, value] of [["api_key_id", key], ["model_id", model]]) expect(new URLSearchParams(page.url().split("?")[1]).get(param!)).toBe(value);
    await page.reload(); await settle(page);
    await expect(keySelect).toHaveValue(key); await expect(modelSelect).toHaveValue(model);
  }
  const constrained = (requests.get(page) ?? []).slice(phaseStart).filter(r => r.method === "GET" && ["/api/usage/metrics", "/api/usage/calls", "/api/usage/summary"].includes(r.path));
  await test.info().attach("all-requests-before-explicit-filter-clear", { body: JSON.stringify(constrained), contentType: "application/json" });
  for (const path of ["/api/usage/metrics", "/api/usage/calls", "/api/usage/summary"]) expect(constrained.filter(r => r.path === path).length, path).toBeGreaterThanOrEqual(1);
  for (const read of constrained) {
    const query = new URLSearchParams(read.query);
    expect(query.get("api_key_id"), read.path + read.query).toBe(key);
    if (read.path !== "/api/usage/summary") expect(query.get("model_id"), read.path + read.query).toBe(model);
  }
  const keySelect = page.getByRole("combobox", { name: "密钥", exact: true });
  const modelSelect = page.getByRole("combobox", { name: "模型", exact: true });
  const keyClearStart = (requests.get(page) ?? []).length;
  await keySelect.selectOption(""); await settle(page);
  const keyCleared = (requests.get(page) ?? []).slice(keyClearStart).filter(r => r.method === "GET" && r.path === "/api/usage/calls");
  expect(keyCleared.length).toBeGreaterThanOrEqual(1);
  for (const read of keyCleared) { expect(new URLSearchParams(read.query).has("api_key_id")).toBe(false); expect(new URLSearchParams(read.query).get("model_id")).toBe(model); }
  const modelClearStart = (requests.get(page) ?? []).length;
  await modelSelect.selectOption(""); await settle(page);
  const allCleared = (requests.get(page) ?? []).slice(modelClearStart).filter(r => r.method === "GET" && r.path === "/api/usage/calls");
  expect(allCleared.length).toBeGreaterThanOrEqual(1);
  for (const read of allCleared) { expect(new URLSearchParams(read.query).has("api_key_id")).toBe(false); expect(new URLSearchParams(read.query).has("model_id")).toBe(false); }
  await expect(page.getByRole("table", { name: "调用记录", exact: true }).locator("tbody tr")).toHaveCount(4);
});
