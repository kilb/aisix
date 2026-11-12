import { expect, test, type Page } from "@playwright/test";
import { readFileSync, mkdirSync } from "node:fs";
import { join, resolve } from "node:path";
import { spawn } from "node:child_process";

// 只使用保留的真实隔离世界；没有seed、响应替换、模型调用或上游探测。
const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
if (!state.ready || state.mockUpstream !== false) throw new Error("REFINED_QA_SESSION must be a ready, real, isolated stack");
const output = resolve(process.env.REFINED_QA_OUTPUT!);
const app: string = "admin";
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
    const response = await fetch("/admin/logout", { method: "POST", headers: { "content-type": "application/json" }, body: "{}" });
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
async function primaryLink(page: Page, name: string): Promise<void> {
  await page.getByRole("link", { name, exact: true }).click();
  await settle(page);
  await expect(page.getByRole("link", { name, exact: true })).toHaveAttribute("aria-current", "page");
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
  const authorizedRouting = info.title.startsWith("routing-");
  expect.soft((requests.get(page) ?? []).filter(r => r.method !== "GET" && r.method !== "HEAD" && !/^\/(?:api|admin)\/(?:login|logout)$/.test(r.path) && !(authorizedRouting && /^\/admin\/(?:public-models\/[a-f0-9-]+(?:\/routing|\/regenerate)?|provider-keys\/[a-f0-9-]+\/channel-models\/[a-f0-9-]+)$/.test(r.path))), "no unexpected business/probe/model writes").toEqual([]);

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

    await expect(page.getByText("75%", { exact: true })).toBeVisible();
    await expect(page.getByText("¥0.941181", { exact: true })).toBeVisible();
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


test("model-catalogue-public-routes-and-tests-are-independent", async ({ page }) => {
  await login(page); await goto(page, "#models");
  await expect(page.getByRole("heading", { name: "模型目录", exact: true })).toBeVisible();
  await expect(page.getByRole("tablist", { name: "模型测试视图", exact: true })).toHaveCount(0);
  await expect(page.getByRole("table", { name: "已保存测试历史", exact: true })).toHaveCount(0);
  await primaryLink(page, "公共模型与路由");
  expect(page.url().split("#")[1]?.split("?")[0]).toBe("public-routes");
  await expect(page.getByRole("heading", { name: "公共模型与路由", exact: true })).toBeVisible();
  await primaryLink(page, "模型测试");
  expect(page.url().split("#")[1]?.split("?")[0]).toBe("model-tests");
  await expect(page.getByRole("tablist", { name: "模型测试视图", exact: true })).toBeVisible();
  expect((requests.get(page) ?? []).filter(r => r.method === "POST" && /probe/.test(r.path))).toEqual([]);
});

test("legacy-links-map-without-losing-billing-meaning", async ({ page }) => {
  await login(page);
  await goto(page, "#pricing?section=public-models");
  await expect(page.getByRole("heading", { name: "公共模型与路由", exact: true })).toBeVisible();
  await goto(page, "#billing?section=charges&tenant_id=b7928a9a-ba93-4209-92a4-c7fd806ffd9d");
  await expect(page.getByRole("heading", { name: /调用.*记录/ }).first()).toBeVisible();
  expect(new URLSearchParams(page.url().split("?")[1]).get("tenant_id")).toBe("b7928a9a-ba93-4209-92a4-c7fd806ffd9d");
  await goto(page, "#billing");
  await expect(page.getByRole("heading", { name: "账户", exact: true })).toBeVisible();
  await expect(page.getByRole("group", { name: "计费分区", exact: true }).getByRole("button", { name: "账户", exact: true })).toHaveAttribute("aria-pressed", "true");
  expect(page.url().split("#")[1]?.split("?")[0]).toBe("billing");
});

for (const role of ["admin", "operator", "viewer"]) {
  test(`history-${role}-readable-and-start-permission`, async ({ page }) => {
    const actors = JSON.parse(readFileSync(join(session, "current-roles-private.json"), "utf8")).actors;
    const who = role === "admin" ? auth.operator : actors.find((a: any) => a.surface === "admin" && a.role === role);
    expect(who).toBeTruthy();
    await login(page, who); await goto(page, "#model-tests?section=history");
    const tabs = page.getByRole("tablist", { name: "模型测试视图", exact: true });
    await expect(tabs.getByRole("tab", { name: "测试历史", exact: true })).toHaveAttribute("aria-selected", "true");
    await expect(page.getByRole("table", { name: "已保存测试历史", exact: true })).toBeVisible();
    await page.getByRole("combobox", { name: "历史测试目标", exact: true }).selectOption("f7f7d097-beaf-45f8-a66c-5d89ae5e92ab");
    await settle(page);
    const table = page.getByRole("table", { name: "已保存测试历史", exact: true });
    await expect(table).toContainText("12 / 5"); await expect(table).toContainText("102 / 33");
    for (const label of ["JSON", "指令", "算术", "排序"]) await expect(table).toContainText(label);
    const reply = await browserGet(page, baseUrl + "/admin/model-probes?model_id=f7f7d097-beaf-45f8-a66c-5d89ae5e92ab&mode=capability&limit=10");
    expect(reply.status()).toBe(200); const history = (await reply.json()).items.find((x: any) => x.id === "2");
    expect(history).toEqual(JSON.parse(readFileSync(join(session, "expected-history.json"), "utf8")));
    await tabs.getByRole("tab", { name: "当前测试", exact: true }).click(); await settle(page);
    const start = page.getByRole("button", { name: /^(?:重新)?测试所有直连模型$/ });
    await expect(start).toHaveCount(role === "admin" ? 1 : 0);
    await tabs.getByRole("tab", { name: "测试历史", exact: true }).click(); await settle(page);
    await expect(page.getByRole("combobox", { name: "历史测试目标", exact: true })).toHaveValue("f7f7d097-beaf-45f8-a66c-5d89ae5e92ab");
    expect((requests.get(page) ?? []).filter(r => r.method === "POST" && /probe/.test(r.path))).toEqual([]);
  });
}

test("routing-saves-weighted-api-key-affinity-and-roundtrip", async ({ page }) => {
  const routeSession = resolve(process.env.REFINED_QA_ROUTING_SESSION!);
  const routeState = JSON.parse(readFileSync(join(routeSession, "public.json"), "utf8"));
  const routeAuth = JSON.parse(readFileSync(join(routeSession, "local-auth.json"), "utf8"));
  const fixture = JSON.parse(readFileSync(join(routeSession, "routing-fixture.json"), "utf8"));
  if (!routeState.ready || routeState.mockUpstream !== false || routeState.database !== "refined_ui_1112_qa6_cp") throw new Error("Dedicated real routing configuration world required");
  const url = routeState.adminUrl;
  await login(page, routeAuth.operator, url);
  await page.goto(url + "/#public-routes"); await settle(page);
  await page.getByRole("button", { name: fixture.publicModel.name, exact: true }).click(); await settle(page);
  const form = page.getByRole("form").filter({ has: page.getByRole("combobox", { name: "同级分配方式", exact: true }) });
  await expect(form).toHaveCount(1);
  const mode = form.getByRole("combobox", { name: "同级分配方式", exact: true });
  const affinityValue = await mode.getByRole("option", { name: /加权一致性哈希.*API key/ }).getAttribute("value");
  expect(affinityValue).toBeTruthy(); await mode.selectOption(affinityValue!);
  await form.getByRole("button", { name: "保存", exact: true }).click(); await settle(page);
  const response = await browserGet(page, url + `/admin/public-models/${fixture.publicModel.id}`);
  expect(response.status()).toBe(200);
  const saved = await response.json();
  expect(saved.routing.strategy).toBe("consistent_hash");
  expect(saved.routing.hash_on).toEqual([{ type: "api_key" }]);
  const documents = await browserGet(page, url + "/admin/resources/models");
  expect(documents.status()).toBe(200);
  const projected = (await documents.json()).items.find((m: any) => m.id === fixture.publicModel.id);
  expect(projected).toBeTruthy();
  expect(projected.doc.routing.strategy).toBe("consistent_hash");
  expect(projected.doc.routing.hash_on).toEqual([{ type: "api_key" }]);
  expect(projected.doc.routing.targets.map((t: any) => ({ priority: t.priority, weight: t.weight }))).toEqual([
    { priority: 100, weight: 3 }, { priority: 100, weight: 1 }, { priority: 50, weight: 2 }, { priority: 50, weight: 1 },
  ]);
  await page.reload(); await settle(page);
  await expect(page.getByRole("combobox", { name: "同级分配方式", exact: true }).locator("option:checked")).toHaveText(/加权一致性哈希.*API key/);
});


async function holdRoutingRow(database: string, id: string): Promise<() => Promise<void>> {
  if (database !== "refined_ui_1112_qa6_cp" || !/^[a-f0-9-]{36}$/.test(id)) throw new Error("Only dedicated routing fixture rows may be locked");
  const child = spawn("psql", [`postgres://postgres@127.0.0.1:27921/${database}`, "-X", "-Atq", "-v", "ON_ERROR_STOP=1"], { stdio: ["pipe", "pipe", "pipe"] });
  let done = false;
  const closed = new Promise<void>(resolve => child.once("exit", () => { done = true; resolve(); }));
  const release = async (): Promise<void> => { if (!done) child.stdin.end("ROLLBACK;\n\\q\n"); await closed; };
  const timer = setTimeout(() => { void release(); }, 2400);
  const ready = new Promise<void>((resolve, reject) => {
    let data = ""; let observedReady = false;
    child.stdout.on("data", bytes => { data += bytes.toString(); if (data.includes("routing-lock-ready")) { observedReady = true; resolve(); } });
    child.once("error", reject); child.once("exit", code => { if (!observedReady || code !== 0) reject(new Error("Dedicated row lock process failed")); });
  });
  child.stdin.write(`BEGIN; SET LOCAL idle_in_transaction_session_timeout='2800ms'; SET LOCAL statement_timeout='2700ms'; SELECT id FROM public_models WHERE id='${id}' FOR UPDATE; SELECT id FROM channel_models WHERE public_model_id='${id}' FOR UPDATE; SELECT 'routing-lock-ready';\n`);
  try { await ready; } catch (error) { clearTimeout(timer); await release(); throw error; }
  return async () => { clearTimeout(timer); await release(); };
}

for (const direction of ["save-first", "regenerate-first"]) {
  test(`routing-${direction}-mutual-exclusion`, async ({ page }) => {
    const routeSession = resolve(process.env.REFINED_QA_ROUTING_SESSION!);
    const routeState = JSON.parse(readFileSync(join(routeSession, "public.json"), "utf8"));
    const routeAuth = JSON.parse(readFileSync(join(routeSession, "local-auth.json"), "utf8"));
    const fixture = JSON.parse(readFileSync(join(routeSession, "routing-fixture.json"), "utf8"));
    const url = routeState.adminUrl;
    await login(page, routeAuth.operator, url);
    await page.goto(url + "/#public-routes"); await settle(page); await page.getByRole("button", { name: fixture.publicModel.name, exact: true }).click(); await settle(page);
    const form = page.getByRole("form", { name: `编辑模型「${fixture.publicModel.name}」`, exact: true });
    const firstTarget = form.getByRole("listitem").filter({ hasText: `账号：${fixture.accounts[0].name}` });
    await expect(firstTarget).toHaveCount(1);
    if (direction === "save-first") { await firstTarget.getByLabel("优先级", { exact: true }).fill("120"); await firstTarget.getByLabel("权重", { exact: true }).fill("5"); }
    else { await firstTarget.getByLabel("权重", { exact: true }).fill("6"); }
    const save = form.getByRole("button", { name: "保存", exact: true });
    const regenerate = page.getByRole("button", { name: "重新生成", exact: true });
    await expect(save).toBeEnabled(); await expect(regenerate).toBeEnabled();
    const release = await holdRoutingRow(routeState.database, fixture.publicModel.id);
    try {
      const pending = page.waitForRequest(r => r.method() !== "GET" && r.url().includes(fixture.publicModel.id));
      await (direction === "save-first" ? save : regenerate).click(); await pending;
      await expect(direction === "save-first" ? regenerate : save).toBeDisabled({ timeout: 1000 });
    } finally { await release(); }
    await settle(page); await expect(regenerate).toBeEnabled();
    const response = await browserGet(page, url + `/admin/resources/models/${fixture.publicModel.id}`); expect(response.status()).toBe(200);
    const projected = (await response.json()).doc.routing;
    expect(projected.strategy).toBe("consistent_hash"); expect(projected.hash_on).toEqual([{ type: "api_key" }]);
    expect(projected.targets.map((t: any) => ({ priority: t.priority, weight: t.weight }))).toEqual([
      { priority: 120, weight: 5 }, { priority: 100, weight: 1 }, { priority: 50, weight: 2 }, { priority: 50, weight: 1 },
    ]);
  });
}


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

test("key-only-analysis-to-calls-shows-id-and-can-clear-filter", async ({ page }) => {
  await login(page);
  const fixture = JSON.parse(readFileSync(join(session, "scope-fixture.json"), "utf8"));
  const key = fixture.usedKeyIds[0]; expect(key).toBeTruthy();
  await goto(page, "#analytics?api_key_id=" + key);
  await page.getByRole("link", { name: "查看调用记录", exact: true }).first().click(); await settle(page);
  expect(page.url().split("#")[1]?.split("?")[0]).toBe("calls");
  expect(new URLSearchParams(page.url().split("?")[1]).get("api_key_id")).toBe(key);
  const select = page.getByRole("combobox", { name: "密钥", exact: true });
  await expect(select).toHaveValue(key); await expect(select.locator("option:checked")).toContainText(key);
  const beforeClear = (requests.get(page) ?? []).length;
  await select.selectOption(""); await settle(page);
  await expect(select).toHaveValue("");
  expect(new URLSearchParams(page.url().split("?")[1]).has("api_key_id")).toBe(false);
  const reads = (requests.get(page) ?? []).slice(beforeClear).filter(r => r.path === "/admin/pricing/charges");
  expect(reads.length).toBeGreaterThan(0); expect(new URLSearchParams(reads.at(-1)!.query).has("api_key_id")).toBe(false);
});
