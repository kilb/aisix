import { expect, test, type Page } from "@playwright/test";
import { mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

// 操作独立真实控制面；不截获网络，不调用付费模型或改写价格。
const session = resolve(process.env.KEY_MANAGEMENT_QA_SESSION!);
const output = resolve(process.env.KEY_MANAGEMENT_QA_OUTPUT!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
if (state.ready !== true || state.realBackend !== true || state.mockUpstream !== false || process.env.KEY_MANAGEMENT_ALLOW_WRITES !== "isolated") throw new Error("Key management E2E requires an explicitly writable isolated real CP world");
if (!["127.0.0.1", "localhost"].includes(new URL(state.adminUrl).hostname)) throw new Error("Key E2E accepts loopback only");
mkdirSync(output, { recursive: true });
interface Tenant { id: string; is_internal: boolean; effective_model_ids: string[]; status: string }
interface Group { id: string; name: string; display_name: string; provider_keys: unknown[] }
interface Model { id: string; doc: { display_name?: string } }
interface Key { id: string; label: string; model_ids: string[] | null; allowed_models: string[] | null; rate_limit: { concurrency?: number; rpm?: number }; disabled: boolean; expires_at: string | null; group: { id: string } | null; version: number }
interface Budget { id: string; scope: string; target: { id: string }; set_by: string; kind: string; period: string | null; limit_micro_cny: number }
async function request<T>(page: Page, method: string, path: string, body?: unknown, version?: number): Promise<{ status: number; body: T }> {
  return page.evaluate(async ({ method, path, body, version }) => {
    const response = await fetch(path, { method, credentials: "include", headers: { ...(body === undefined ? {} : { "Content-Type": "application/json" }), ...(version === undefined ? {} : { "If-Match": `"${version}"` }) }, ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
    return { status: response.status, body: response.status === 204 ? null : await response.json() };
  }, { method, path, body, version });
}
async function get<T>(page: Page, path: string): Promise<T> { const response = await request<T>(page, "GET", path); expect(response.status, path).toBe(200); return response.body; }
async function login(page: Page, target = state.adminUrl): Promise<void> {
  await page.goto(target);
  await page.getByLabel("邮箱", { exact: true }).fill(auth.operator.email);
  await page.getByLabel("口令", { exact: true }).fill(auth.operator.password);
  const response = page.waitForResponse(r => new URL(r.url()).pathname === "/admin/login" && r.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await response).status()).toBe(200);
  await page.waitForLoadState("networkidle");
}
async function choose(page: Page, label: string, name: string): Promise<void> {
  await page.getByRole("combobox", { name: label, exact: true }).click();
  await page.getByRole("option", { name, exact: true }).click();
}
async function world(page: Page): Promise<{ tenant: Tenant; group: Group; models: Model[] }> {
  const tenants = await get<{ items: Tenant[] }>(page, "/admin/tenants");
  const groups = await get<{ items: Group[] }>(page, "/admin/provider-groups");
  const catalog = await get<{ items: Model[] }>(page, "/admin/resources/models");
  const tenant = tenants.items.find(item => item.is_internal);
  expect(tenant, "real internal tenant").toBeDefined();
  const group = groups.items.find(item => item.provider_keys.length > 0) ?? groups.items[0];
  expect(group, "real available group").toBeDefined();
  const models = catalog.items.filter(model => tenant!.effective_model_ids.includes(model.id));
  expect(models.length, "two real model choices are needed").toBeGreaterThanOrEqual(2);
  return { tenant: tenant!, group: group!, models };
}
function name(model: Model): string { return model.doc.display_name || model.id; }
async function settle(page: Page): Promise<void> {
  await page.waitForLoadState("networkidle");
  await page.evaluate(() => document.fonts.ready);
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
  expect(await page.title()).not.toBe("");
}
async function keys(page: Page, tenant: string): Promise<Key[]> { return (await get<{ items: Key[] }>(page, `/admin/tenants/${tenant}/keys`)).items; }
async function budgets(page: Page, tenant: string, keyId: string): Promise<Budget[]> { return (await get<{ items: Budget[] }>(page, `/admin/tenants/${tenant}/budgets`)).items.filter(row => row.scope === "api_key" && row.target.id === keyId); }

test.beforeEach(async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", error => errors.push(error.message));
  await login(page);
  (page as Page & { keyErrors?: string[] }).keyErrors = errors;
});
test.afterEach(async ({ page }) => { expect((page as Page & { keyErrors?: string[] }).keyErrors ?? []).toEqual([]); });

for (const width of [1440, 390]) {
  test(`管理员创建额度并编辑已分组密钥 ${width}`, async ({ page }) => {
    await page.setViewportSize({ width, height: 1000 });
    const { tenant, group, models } = await world(page);
    const label = `key-469-${width}-${Date.now()}`;
    await page.goto(`${state.adminUrl}/#keys?tenant=${tenant.id}`);
    await page.getByRole("button", { name: "新建密钥", exact: true }).click();
    const dialog = page.getByRole("dialog");
    await dialog.getByLabel("名称", { exact: true }).fill(label);
    await choose(page, "分组（必选）", group.display_name === group.name ? group.display_name : `${group.display_name}（${group.name}）`);
    await choose(page, "模型范围", "指定模型");
    await dialog.getByLabel("搜索模型", { exact: true }).fill(name(models[0]!));
    await dialog.getByRole("checkbox", { name: name(models[0]!), exact: true }).check();
    await dialog.getByLabel("费用额度（元）", { exact: true }).fill("12.34");
    await choose(page, "额度周期", "累计");
    await dialog.getByLabel("并发", { exact: true }).fill("2");
    await dialog.getByLabel("每分钟请求", { exact: true }).fill("24");
    const minted = page.waitForResponse(r => new URL(r.url()).pathname === `/admin/tenants/${tenant.id}/keys` && r.request().method() === "POST");
    await dialog.getByRole("button", { name: "创建", exact: true }).click();
    if (group.provider_keys.length === 0) await dialog.getByRole("button", { name: "确认创建", exact: true }).click();
    const created = await minted;
    expect(created.status()).toBe(201);
    const key = (await created.json() as { key: Key }).key;
    expect(key.model_ids).toEqual([models[0]!.id]);
    expect(key.allowed_models).toBeNull();
    expect(key.group?.id).toBe(group.id);
    const initialBudgets = await budgets(page, tenant.id, key.id);
    expect(initialBudgets).toHaveLength(1);
    expect(initialBudgets[0]).toMatchObject({ set_by: "operator", kind: "cumulative", period: null, limit_micro_cny: 12_340_000 });
    await dialog.getByRole("button", { name: "关闭", exact: true }).click();
    await page.getByLabel("密钥列表搜索", { exact: true }).fill(label);
    const row = page.getByRole("row").filter({ has: page.getByRole("link", { name: label, exact: true }) });
    await row.getByRole("button", { name: "编辑密钥", exact: true }).click();
    await expect(page).toHaveURL(new RegExp(`key=${key.id}`));
    await expect(page.getByRole("heading", { name: "编辑密钥", exact: true })).toBeVisible();
    await page.getByLabel("名称", { exact: true }).fill(`${label}-edited`);
    await page.getByLabel("搜索模型", { exact: true }).fill(name(models[0]!));
    await page.getByRole("checkbox", { name: name(models[0]!), exact: true }).uncheck();
    await page.getByLabel("搜索模型", { exact: true }).fill(name(models[1]!));
    await page.getByRole("checkbox", { name: name(models[1]!), exact: true }).check();
    await page.getByLabel("并发", { exact: true }).fill("3");
    await page.getByLabel("每分钟请求", { exact: true }).fill("36");
    await page.getByLabel("过期时间", { exact: true }).fill("2032-01-02T03:04");
    await choose(page, "平台状态", "停用");
    const updated = page.waitForResponse(r => new URL(r.url()).pathname === `/admin/keys/${key.id}` && r.request().method() === "PATCH");
    await page.getByRole("button", { name: "保存配置", exact: true }).click();
    expect((await updated).status()).toBe(200);
    await page.reload(); await settle(page);
    const stored = (await keys(page, tenant.id)).find(item => item.id === key.id)!;
    expect(stored).toMatchObject({ label: `${label}-edited`, model_ids: [models[1]!.id], allowed_models: null, rate_limit: { concurrency: 3, rpm: 36 }, group: { id: group.id }, disabled: true });
    expect(Date.parse(stored.expires_at!)).toBe(Date.parse("2032-01-01T19:04:00Z"));
    await choose(page, "平台状态", "启用");
    await page.getByLabel("过期时间", { exact: true }).fill("");
    const cleared = page.waitForResponse(r => new URL(r.url()).pathname === `/admin/keys/${key.id}` && r.request().method() === "PATCH");
    await page.getByRole("button", { name: "保存配置", exact: true }).click(); expect((await cleared).status()).toBe(200);
    expect((await keys(page, tenant.id)).find(item => item.id === key.id)).toMatchObject({ disabled: false, expires_at: null });
    const quotaRow = page.getByRole("form", { name: "累计额度", exact: true });
    await expect(quotaRow.getByLabel("累计额度（元）", { exact: true })).toHaveValue("12.34");
    await quotaRow.getByLabel("累计额度（元）", { exact: true }).fill("23.45");
    const savedBudget = page.waitForResponse(r => new URL(r.url()).pathname.endsWith(`/budgets/${initialBudgets[0]!.id}`) && r.request().method() === "PATCH");
    await quotaRow.getByRole("button", { name: "保存额度", exact: true }).click(); expect((await savedBudget).status()).toBe(200);
    expect((await budgets(page, tenant.id, key.id))[0]!.limit_micro_cny).toBe(23_450_000);
    const removed = page.waitForResponse(r => new URL(r.url()).pathname.endsWith(`/budgets/${initialBudgets[0]!.id}`) && r.request().method() === "DELETE");
    await quotaRow.getByRole("button", { name: "移除额度", exact: true }).click();
    await quotaRow.getByRole("button", { name: "确认移除额度", exact: true }).click(); expect((await removed).status()).toBe(204);
    expect(await budgets(page, tenant.id, key.id)).toEqual([]);
    const add = page.getByRole("form", { name: "添加密钥费用额度", exact: true });
    await add.getByLabel("费用额度（元）", { exact: true }).fill("3.01");
    await choose(page, "额度周期", "每月");
    const added = page.waitForResponse(r => new URL(r.url()).pathname === `/admin/tenants/${tenant.id}/budgets` && r.request().method() === "POST");
    await add.getByRole("button", { name: "添加额度", exact: true }).click(); expect((await added).status()).toBe(201);
    expect((await budgets(page, tenant.id, key.id))[0]).toMatchObject({ set_by: "operator", kind: "periodic", period: "month", limit_micro_cny: 3_010_000 });
    await settle(page);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)).toBe(true);
    await page.screenshot({ path: join(output, `admin-editor-${width}.png`) });
  });
}

test("编辑历史通配密钥保留原授权，显式切换模型时清除覆盖", async ({ page }) => {
  const { tenant, group, models } = await world(page);
  const label = `legacy-pattern-469-${Date.now()}`;
  const patterns = [`${name(models[0]!)}*`];
  const created = await request<{ key: Key }>(page, "POST", `/admin/tenants/${tenant.id}/keys`, { label, allowed_models: patterns, group_id: group.id });
  expect(created.status).toBe(201); const id = created.body.key.id;
  await page.context().clearCookies();
  await login(page, `${state.adminUrl}/#keys?tenant=${tenant.id}&key=${id}`);
  await expect(page).toHaveURL(new RegExp(`key=${id}`));
  await expect(page.getByLabel("模型名称模式", { exact: true })).toHaveValue(patterns.join(", "));
  await page.getByLabel("名称", { exact: true }).fill(`${label}-renamed`);
  const rename = page.waitForResponse(r => new URL(r.url()).pathname === `/admin/keys/${id}` && r.request().method() === "PATCH");
  await page.getByRole("button", { name: "保存配置", exact: true }).click(); expect((await rename).status()).toBe(200);
  expect((await keys(page, tenant.id)).find(item => item.id === id)!.allowed_models).toEqual(patterns);
  await choose(page, "模型范围", "指定模型");
  await page.getByLabel("搜索模型", { exact: true }).fill(name(models[1]!));
  await page.getByRole("checkbox", { name: name(models[1]!), exact: true }).check();
  const selected = page.waitForResponse(r => new URL(r.url()).pathname === `/admin/keys/${id}` && r.request().method() === "PATCH");
  await page.getByRole("button", { name: "保存配置", exact: true }).click(); expect((await selected).status()).toBe(200);
  expect((await keys(page, tenant.id)).find(item => item.id === id)).toMatchObject({ model_ids: [models[1]!.id], allowed_models: null });
  await choose(page, "模型范围", "不限制模型");
  const inherited = page.waitForResponse(r => new URL(r.url()).pathname === `/admin/keys/${id}` && r.request().method() === "PATCH");
  await page.getByRole("button", { name: "保存配置", exact: true }).click(); expect((await inherited).status()).toBe(200);
  expect((await keys(page, tenant.id)).find(item => item.id === id)).toMatchObject({ model_ids: null, allowed_models: null });
  await choose(page, "模型范围", "名称模式（高级）");
  await page.getByLabel("模型名称模式", { exact: true }).fill("");
  const empty = page.waitForResponse(r => new URL(r.url()).pathname === `/admin/keys/${id}` && r.request().method() === "PATCH");
  await page.getByRole("button", { name: "保存配置", exact: true }).click(); expect((await empty).status()).toBe(200);
  expect((await keys(page, tenant.id)).find(item => item.id === id)!.allowed_models).toEqual([]);
  await page.getByRole("button", { name: "返回密钥列表", exact: true }).click();
  await page.getByLabel("密钥列表搜索", { exact: true }).fill(`${label}-renamed`);
  const row = page.getByRole("row").filter({ has: page.getByRole("link", { name: `${label}-renamed`, exact: true }) });
  const column = (await page.getByRole("columnheader").allTextContents()).findIndex(title => title.trim() === "模型");
  expect(column).toBeGreaterThanOrEqual(0);
  await expect(row.getByRole("cell").nth(column)).toHaveText("无");
});

test("切换同租户的另一密钥不会沿用额度草稿或错误", async ({ page }) => {
  const { tenant } = await world(page);
  const listed = await keys(page, tenant.id);
  expect(listed.length, "real same-tenant keys for a navigation regression").toBeGreaterThanOrEqual(2);
  const writes: string[] = [];
  page.on("request", request => { if (["POST", "PATCH", "PUT", "DELETE"].includes(request.method())) writes.push(request.method() + " " + new URL(request.url()).pathname); });
  await page.goto(`${state.adminUrl}/#keys?tenant=${tenant.id}&key=${listed[0]!.id}`);
  const add = page.getByRole("form", { name: "添加密钥费用额度", exact: true });
  await add.getByLabel("费用额度（元）", { exact: true }).fill("0");
  await add.getByRole("button", { name: "添加额度", exact: true }).click();
  await expect(add.getByRole("alert")).toHaveText("请输入大于零的费用额度，最多两位小数");
  await add.getByLabel("费用额度（元）", { exact: true }).fill("789.123456");
  await choose(page, "额度周期", "每周");
  await page.evaluate(hash => { window.location.hash = hash; }, `#keys?tenant=${tenant.id}&key=${listed[1]!.id}`);
  await expect(page).toHaveURL(new RegExp(`key=${listed[1]!.id}`));
  await expect(add.getByLabel("费用额度（元）", { exact: true })).toHaveValue("");
  await expect(add.getByRole("combobox", { name: "额度周期", exact: true })).toHaveText("累计");
  await expect(add.getByRole("alert")).toHaveCount(0);
  expect(writes, "changing editors does not submit either key's quota").toEqual([]);
});

test("客户租户的创建界面只提供授权模型选择", async ({ page }) => {
  const listed = await get<{ items: Tenant[] }>(page, "/admin/tenants");
  const customer = listed.items.find(tenant => !tenant.is_internal && tenant.status === "active");
  expect(customer, "retained active customer tenant").toBeDefined();
  const writes: string[] = [];
  page.on("request", request => { if (["POST", "PATCH", "PUT", "DELETE"].includes(request.method())) writes.push(request.method() + " " + new URL(request.url()).pathname); });
  await page.goto(`${state.adminUrl}/#keys?tenant=${customer!.id}`);
  await page.getByRole("button", { name: "新建密钥", exact: true }).click();
  await page.getByRole("combobox", { name: "模型范围", exact: true }).click();
  await expect(page.getByRole("option", { name: "名称模式（高级）", exact: true })).toHaveCount(0);
  await expect(page.getByRole("option", { name: "不限制模型", exact: true })).toBeVisible();
  await page.getByRole("option", { name: "指定模型", exact: true }).click();
  await expect(page.getByLabel("模型名称模式", { exact: true })).toHaveCount(0);
  expect(writes, "viewing model choices performs no mutation").toEqual([]);
});

test("版本冲突保留服务端配置，重新载入后才能保存", async ({ page }) => {
  const { tenant, group } = await world(page);
  const label = `conflict-469-${Date.now()}`;
  const created = await request<{ key: Key }>(page, "POST", `/admin/tenants/${tenant.id}/keys`, { label, group_id: group.id });
  expect(created.status).toBe(201); const key = created.body.key;
  await page.goto(`${state.adminUrl}/#keys?tenant=${tenant.id}&key=${key.id}`);
  await expect(page.getByLabel("名称", { exact: true })).toHaveValue(label);
  const external = await request<Key>(page, "PATCH", `/admin/keys/${key.id}`, { label: `${label}-other` }, key.version);
  expect(external.status).toBe(200);
  await page.getByLabel("名称", { exact: true }).fill(`${label}-mine`);
  const conflict = page.waitForResponse(response => new URL(response.url()).pathname === `/admin/keys/${key.id}` && response.request().method() === "PATCH");
  await page.getByRole("button", { name: "保存配置", exact: true }).click(); expect((await conflict).status()).toBe(412);
  await expect(page.getByRole("alert").filter({ hasText: "该密钥已被其他人修改" })).toBeVisible();
  expect((await get<Key>(page, `/admin/keys/${key.id}`)).label).toBe(`${label}-other`);
  await page.getByRole("button", { name: "重新载入配置", exact: true }).click();
  await expect(page.getByLabel("名称", { exact: true })).toHaveValue(`${label}-other`);
  await page.getByLabel("名称", { exact: true }).fill(`${label}-mine`);
  const saved = page.waitForResponse(response => new URL(response.url()).pathname === `/admin/keys/${key.id}` && response.request().method() === "PATCH");
  await page.getByRole("button", { name: "保存配置", exact: true }).click(); expect((await saved).status()).toBe(200);
  expect((await get<Key>(page, `/admin/keys/${key.id}`)).label).toBe(`${label}-mine`);
});
