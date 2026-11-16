import { expect, test, type Locator, type Page } from "@playwright/test";
import { randomUUID } from "node:crypto";
import { mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { chooseSelect } from "../e2e/select";

// 使用同一真实控制面与账号配置专属模型，只保存配置，不发送上游请求。
const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
const baseUrl: string = state.adminUrl;
const accountId: string = state.tokenUiFixture?.providerAccountId;
const upstream: string = state.tokenUiFixture?.publicModelName;
if (!state.ready || state.mockUpstream !== false || !accountId || !upstream || !["127.0.0.1", "localhost"].includes(new URL(baseUrl).hostname)) {
  throw new Error("Session routing E2E requires the real isolated loopback fixture");
}
const output = resolve(process.env.REFINED_QA_OUTPUT!);
const sessionSources = ["x-claude-code-session-id", "session-id", "thread-id"].map(name => ({ type: "header", name }));
const sessionOption = "会话固定 · 同一会话使用同一上游账号";

async function api(page: Page, method: string, path: string, body?: unknown, version?: number): Promise<any> {
  if (!path.startsWith("/admin/")) throw new Error("Only the local configuration API is allowed");
  const response = await page.evaluate(async ({ method, path, body, version }) => {
    const result = await fetch(path, { method, credentials: "include", headers: {
      "content-type": "application/json", ...(version === undefined ? {} : { "if-match": String(version) }),
    }, ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
    return { status: result.status, body: result.status === 204 ? null : await result.json() };
  }, { method, path, body, version });
  expect([200, 201, 204], `${method} ${path}`).toContain(response.status);
  return response.body;
}

async function settle(page: Page): Promise<void> {
  await page.evaluate(() => document.fonts.ready);
  await page.waitForLoadState("networkidle");
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
}

async function openPublic(page: Page, name: string): Promise<Locator> {
  await page.goto(`${baseUrl}/#public-routes`);
  await settle(page);
  await page.getByRole("searchbox", { name: "搜索公共模型", exact: true }).fill(name);
  await page.getByRole("button", { name, exact: true }).click();
  const form = page.getByRole("form", { name: `编辑模型「${name}」`, exact: true });
  await expect(form).toBeVisible();
  return form;
}

async function save(page: Page, form: Locator, path: string, method: string): Promise<void> {
  const response = page.waitForResponse(result => new URL(result.url()).pathname === path && result.request().method() === method);
  await form.getByRole("button", { name: "保存", exact: true }).click();
  expect((await response).status()).toBe(200);
  await settle(page);
}

test("会话固定预设经公共与手工路由保存和重开，退出预设恢复原切换行为", async ({ page }) => {
  const errors: string[] = [];
  const upstreamRequests: string[] = [];
  let authenticated = false;
  page.on("pageerror", error => errors.push(error.message));
  page.on("console", message => {
    const bootstrap = !authenticated && message.text().includes("401") && /\/admin\/(me|session)$/.test(message.location().url);
    if (message.type() === "error" && !bootstrap) errors.push(message.text());
  });
  page.on("response", response => {
    const path = new URL(response.url()).pathname;
    if (!path.startsWith("/admin/") || response.status() < 400) return;
    if (!authenticated && response.status() === 401 && /^\/admin\/(me|session)$/.test(path)) return;
    errors.push(`HTTP ${response.status()} ${path}`);
  });
  page.on("request", request => {
    const path = new URL(request.url()).pathname;
    if (/\/upstream-models$|\/probe(?:\/|$)|\/model-tests(?:\/|$)|^\/v1\//.test(path)) upstreamRequests.push(path);
  });
  await page.goto(baseUrl);
  await page.getByLabel("邮箱", { exact: true }).fill(auth.operator.email);
  await page.getByLabel("口令", { exact: true }).fill(auth.operator.password);
  const login = page.waitForResponse(response => new URL(response.url()).pathname === "/admin/login" && response.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await login).status()).toBe(200);
  authenticated = true;
  await settle(page);
  const suffix = randomUUID().slice(0, 8);
  const resources: string[] = [];
  let publicId: string | undefined;
  try {
    const name = `session-public-${suffix}`;
    const model = await api(page, "POST", "/admin/public-models", { name });
    publicId = model.id;
    await api(page, "PUT", `/admin/provider-keys/${accountId}/channel-models/${model.id}`, { upstream_model: upstream, priority: 10, weight: 2, enabled: true });
    const form = await openPublic(page, name);
    await chooseSelect(form.getByRole("combobox", { name: "同级分配方式", exact: true }), { label: sessionOption });
    await expect(form).toContainText("多个同优先级、正权重的主用目标才能分流");
    await expect(form).toContainText("不回退 API key 或空值");
    await save(page, form, `/admin/public-models/${model.id}`, "PATCH");
    const stored = await api(page, "GET", `/admin/public-models/${model.id}`);
    expect(stored.routing).toEqual({ strategy: "consistent_hash", hash_on: sessionSources, hash_failover: false });
    expect(stored.effective_routing).toMatchObject(stored.routing);
    expect(stored.served_by).toHaveLength(1);
    expect(stored.served_by[0]).toMatchObject({ provider_key_id: accountId, priority: 10, weight: 2 });
    const projected = await api(page, "GET", `/admin/resources/models/${model.id}`);
    expect(projected.doc.routing).toMatchObject(stored.routing);
    const reopened = await openPublic(page, name);
    await expect(reopened.getByRole("combobox", { name: "同级分配方式", exact: true })).toHaveText(sessionOption);
    mkdirSync(join(output, "screenshots"), { recursive: true });
    await page.screenshot({ path: join(output, "screenshots", "session-public-routing-1440.png"), fullPage: false });
    await chooseSelect(reopened.getByRole("combobox", { name: "同级分配方式", exact: true }), "api_key");
    await save(page, reopened, `/admin/public-models/${model.id}`, "PATCH");
    expect((await api(page, "GET", `/admin/public-models/${model.id}`)).routing).toEqual({ strategy: "consistent_hash", hash_on: [{ type: "api_key" }] });

    const account = await api(page, "GET", `/admin/resources/provider_keys/${accountId}`);
    const direct = await api(page, "POST", "/admin/resources/models", { doc: { display_name: `session-target-${suffix}`, provider: account.doc.provider, provider_key_id: accountId, model_name: upstream } });
    resources.push(direct.id);
    const manualName = `session-routing-${suffix}`;
    const manual = await api(page, "POST", "/admin/resources/models", { doc: { display_name: manualName, routing: { strategy: "round_robin", targets: [{ model_id: direct.id, priority: 0, weight: 1 }] } } });
    resources.push(manual.id);
    await page.goto(`${baseUrl}/#models`);
    await settle(page);
    await page.getByRole("searchbox", { name: "搜索模型", exact: true }).fill(manualName);
    await page.getByRole("row").filter({ has: page.getByText(manualName, { exact: true }) }).getByRole("button", { name: "编辑", exact: true }).click();
    const manualForm = page.getByRole("form", { name: `编辑模型「${manualName}」`, exact: true });
    await chooseSelect(manualForm.getByRole("combobox", { name: "同级分配方式", exact: true }), "session");
    await save(page, manualForm, `/admin/resources/models/${manual.id}`, "PUT");
    const manualStored = await api(page, "GET", `/admin/resources/models/${manual.id}`);
    expect(manualStored.doc.routing).toEqual({ strategy: "consistent_hash", hash_on: sessionSources, hash_failover: false, targets: [{ model_id: direct.id, priority: 0, weight: 1 }] });
    await page.getByRole("row").filter({ has: page.getByText(manualName, { exact: true }) }).getByRole("button", { name: "编辑", exact: true }).click();
    await expect(manualForm.getByRole("combobox", { name: "同级分配方式", exact: true })).toHaveText(sessionOption);
    await page.setViewportSize({ width: 390, height: 1000 });
    await manualForm.getByRole("heading").first().scrollIntoViewIfNeeded();
    await page.screenshot({ path: join(output, "screenshots", "session-manual-routing-390.png"), fullPage: false });
    await chooseSelect(manualForm.getByRole("combobox", { name: "同级分配方式", exact: true }), "api_key");
    await save(page, manualForm, `/admin/resources/models/${manual.id}`, "PUT");
    expect((await api(page, "GET", `/admin/resources/models/${manual.id}`)).doc.routing).toEqual({ strategy: "consistent_hash", hash_on: [{ type: "api_key" }], targets: [{ model_id: direct.id, priority: 0, weight: 1 }] });
  } finally {
    // 只清理本用例正式创建的标识，先删引用方；不碰共享付费模型或账号。
    for (const id of resources.reverse()) {
      const current = await api(page, "GET", `/admin/resources/models/${id}`);
      await api(page, "DELETE", `/admin/resources/models/${id}`, undefined, current.version);
    }
    if (publicId) await api(page, "POST", `/admin/public-models/${publicId}/delete`, {});
    await test.info().attach("runtime-health", { body: JSON.stringify({ errors, upstreamRequests }), contentType: "application/json" });
  }
  expect(errors).toEqual([]);
  expect(upstreamRequests, "no inference or upstream discovery requests").toEqual([]);
});
