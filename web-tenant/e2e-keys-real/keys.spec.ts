import { expect, test, type Page } from "@playwright/test";
import { readFileSync, mkdirSync } from "node:fs";
import { join, resolve } from "node:path";
import { randomUUID } from "node:crypto";

const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
if (!state.ready || state.mockUpstream !== false) throw new Error("A ready, isolated real world is required");
const baseUrl: string = state.tenantUrl;
if (!["127.0.0.1", "localhost"].includes(new URL(baseUrl).hostname)) throw new Error("Only isolated loopback URLs are allowed");
const output = resolve(process.env.REFINED_QA_OUTPUT!);
const errors = new WeakMap<Page, string[]>();
const authenticated = new WeakSet<Page>();
const expectedRefusals = new WeakMap<Page, Set<string>>();

function expectedHttp(page: Page, path: string, status: number): boolean {
  return (!authenticated.has(page) && status === 401 && ["/api/me", "/api/session"].includes(path)) || expectedRefusals.get(page)?.has(`${status}:${path}`) === true;
}

async function api(page: Page, path: string, method = "GET", body?: object, version?: number): Promise<{ status: number; body: any }> {
  return page.evaluate(async ({ path, method, body, version }) => {
    const response = await fetch(path, {
      method, credentials: "include", headers: { "content-type": "application/json", ...(version === undefined ? {} : { "if-match": `"${version}"` }) },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    const data = response.status === 204 ? null : await response.json();
    // 创建结果只把公开元数据交给测试，明文不进入日志、附件或截图。
    return { status: response.status, body: path === "/api/keys" && method === "POST" ? { key: data.key } : data };
  }, { path, method, body, version });
}

async function login(page: Page): Promise<void> {
  const who = auth.keysOwner ?? auth.metricsOwner;
  if (!who) throw new Error("local-auth.json must provide keysOwner or metricsOwner");
  await page.goto(baseUrl);
  await page.getByLabel("邮箱", { exact: true }).fill(who.email);
  await page.getByLabel("口令", { exact: true }).fill(who.password);
  const response = page.waitForResponse(r => new URL(r.url()).pathname === "/api/login" && r.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await response).status()).toBe(200);
  authenticated.add(page);
  await page.waitForLoadState("networkidle");
}

async function disposableKey(page: Page): Promise<{ key: any; model: any; group: any }> {
  const groups = await api(page, "/api/groups");
  const models = await api(page, "/api/models");
  expect(groups.status).toBe(200); expect(models.status).toBe(200);
  expect(groups.body.items.length).toBeGreaterThan(0); expect(models.body.items.length).toBeGreaterThan(0);
  const group = groups.body.items[0];
  const model = models.body.items[0];
  const created = await api(page, "/api/keys", "POST", { label: `密钥页面验收-${randomUUID().slice(0, 8)}`, group_id: group.id });
  expect(created.status).toBe(201);
  return { key: created.body.key, model, group };
}

async function openEdit(page: Page, id: string): Promise<void> {
  await page.goto(`${baseUrl}/#keys?key=${encodeURIComponent(id)}&edit=1`);
  await expect(page.getByRole("heading", { name: "修改密钥", exact: true })).toBeVisible();
  await expect(page.getByLabel("名称", { exact: true })).toBeVisible();
  await page.waitForLoadState("networkidle");
}

function money(value: string): string {
  const amount = BigInt(value);
  const abs = amount < 0n ? -amount : amount;
  const whole = (abs / 1_000_000n).toString().replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  const fraction = (abs % 1_000_000n).toString().padStart(6, "0").replace(/0+$/, "").padEnd(2, "0");
  return `${amount < 0n ? "-" : ""}¥${whole}.${fraction}`;
}

test.beforeEach(async ({ page }) => {
  const failures: string[] = [];
  errors.set(page, failures);
  expectedRefusals.set(page, new Set());
  page.on("pageerror", error => failures.push(error.message));
  page.on("response", response => {
    const path = new URL(response.url()).pathname;
    if (path.startsWith("/api/") && response.status() >= 400 && !expectedHttp(page, path, response.status())) failures.push(`HTTP ${response.status()} ${path}`);
  });
  page.on("console", message => {
    if (message.type() !== "error") return;
    const location = message.location().url;
    const path = /^https?:/.test(location) ? new URL(location).pathname : "";
    const status = /(?:status of|HTTP)\s+(\d{3})/.exec(message.text())?.[1];
    if (status && expectedHttp(page, path, Number(status))) return;
    failures.push(message.text());
  });
});
test.afterEach(async ({ page }, info) => {
  mkdirSync(join(output, "screenshots"), { recursive: true });
  await page.screenshot({ path: join(output, "screenshots", `keys-${info.title.replace(/[^a-zA-Z0-9]+/g, "-")}.png`), fullPage: false });
  expect.soft(errors.get(page), "no runtime errors").toEqual([]);
});

test("list-costs-use-one-authorized-batch-and-detail-keeps-the-same-window", async ({ page }) => {
  await login(page);
  const requests: string[] = [];
  page.on("request", request => { if (new URL(request.url()).pathname.startsWith("/api/usage/")) requests.push(request.url()); });
  const costs = page.waitForResponse(response => new URL(response.url()).pathname === "/api/usage/keys");
  await page.goto(`${baseUrl}/#keys`);
  const response = await costs; expect(response.status()).toBe(200);
  const batch = await response.json();
  expect(Object.hasOwn(batch, "from")).toBe(true); expect(Object.hasOwn(batch, "to")).toBe(true);
  // 世界必须包含真实计费记录：全零世界不能证明逐密钥归属或精确金额展示。
  const priced = batch.items.find((item: any) => typeof item.charged_micro_cny === "string" && BigInt(item.charged_micro_cny) > 0n);
  expect(priced, "real world has a nonzero key charge").toBeTruthy();
  const keys = await api(page, "/api/keys");
  const key = keys.body.items.find((item: any) => item.id === priced.api_key_id);
  expect(key).toBeTruthy();
  await page.getByRole("searchbox", { name: "搜索密钥", exact: true }).fill(key.label);
  const row = page.getByRole("row").filter({ has: page.locator(`a.key-detail-link[href*="key=${key.id}"]`) });
  await expect(row.locator('td[data-label="已用费用"] > span').first()).toHaveText(money(priced.charged_micro_cny));
  await page.waitForLoadState("networkidle");
  expect(requests.filter(url => new URL(url).pathname === "/api/usage/keys")).toHaveLength(1);
  expect(requests.filter(url => new URL(url).pathname !== "/api/usage/keys")).toEqual([]);
  await row.locator("a.key-detail-link").click();
  await expect(page.getByRole("heading", { name: key.label, exact: true })).toBeVisible();
  const query = new URLSearchParams(page.url().split("?", 2)[1]);
  expect(query.get("key")).toBe(key.id); expect(query.get("from")).toBe(batch.from); expect(query.get("to")).toBe(batch.to);
  await expect(page.getByRole("tabpanel")).toBeVisible();
  await page.getByRole("tab", { name: "费用", exact: true }).click();
  await expect(page.getByRole("tabpanel").getByText(money(priced.charged_micro_cny), { exact: true }).first()).toBeVisible();
  await expect(page.getByRole("region", { name: "密钥计量明细", exact: true })).toBeVisible();
  await expect(page.getByRole("combobox", { name: /每页/ })).toHaveCount(1);
  await page.getByRole("button", { name: "返回密钥列表", exact: true }).click();
  await expect(page.getByRole("heading", { name: "API 密钥", exact: true })).toBeVisible();
  expect(new URLSearchParams(page.url().split("?", 2)[1]).has("key")).toBe(false);
});

test("edit-complete-settings-and-clear-overrides", async ({ page }) => {
  await login(page);
  const { key, model, group } = await disposableKey(page);
  await openEdit(page, key.id);
  await expect(page.getByText(`${group.display_name}（只读）`, { exact: true })).toBeVisible();
  await expect(page.getByText(/密钥创建后不能更换归属成员/)).toBeVisible();
  await expect(page.getByRole("combobox", { name: "归属成员", exact: true })).toHaveCount(0);
  await page.getByLabel("名称", { exact: true }).fill("完整修改参数验收");
  await page.getByRole("radio", { name: "指定模型", exact: true }).check();
  await page.getByRole("button", { name: "展开模型列表", exact: true }).click();
  await page.getByRole("checkbox", { name: model.name, exact: true }).check();
  await page.getByLabel("并发", { exact: true }).fill("1");
  await page.getByLabel("每分钟请求", { exact: true }).fill("1");
  await page.getByLabel("过期时间", { exact: true }).fill("2030-12-01T12:34");
  const status = page.getByRole("combobox", { name: "密钥状态", exact: true });
  await status.click(); await page.getByRole("option", { name: "停用", exact: true }).click();
  const saved = page.waitForResponse(r => new URL(r.url()).pathname === `/api/keys/${key.id}` && r.request().method() === "PATCH");
  await page.getByRole("button", { name: "保存修改", exact: true }).click();
  const response = await saved; expect(response.status()).toBe(200);
  const sent = response.request().postDataJSON();
  expect(sent).toEqual({ label: "完整修改参数验收", model_ids: [model.id], rate_limit: { concurrency: 1, rpm: 1 }, disabled: true, expires_at: "2030-12-01T04:34:00.000Z" });
  expect(response.request().headers()["if-match"]).toBe(`"${key.version}"`);
  await expect(page.getByRole("status")).toHaveText("密钥修改已保存");
  await expect(page.getByRole("status")).toBeInViewport();
  const changed = await api(page, `/api/keys/${key.id}`);
  for (const field of ["label", "model_ids", "rate_limit", "disabled"]) expect(changed.body[field]).toEqual(sent[field]);
  expect(Date.parse(changed.body.expires_at)).toBe(Date.parse(sent.expires_at));
  expect(changed.body.group.id).toBe(group.id);

  await openEdit(page, key.id);
  await page.getByRole("radio", { name: "不限制模型（继承组织授权）", exact: true }).check();
  await page.getByLabel("并发", { exact: true }).fill("");
  await page.getByLabel("每分钟请求", { exact: true }).fill("");
  await page.getByLabel("过期时间", { exact: true }).fill("");
  const cleared = page.waitForResponse(r => new URL(r.url()).pathname === `/api/keys/${key.id}` && r.request().method() === "PATCH");
  await page.getByRole("button", { name: "保存修改", exact: true }).click();
  const clearedResponse = await cleared; expect(clearedResponse.status()).toBe(200);
  expect(clearedResponse.request().postDataJSON()).toEqual({ model_ids: null, rate_limit: null, expires_at: null });
  const inherited = await api(page, `/api/keys/${key.id}`);
  expect(inherited.body.model_ids).toBeNull(); expect(inherited.body.rate_limit).toEqual({}); expect(inherited.body.expires_at).toBeNull();
  expect(inherited.body.disabled).toBe(true);
});

test("stale-settings-never-overwrite-the-newer-key", async ({ page }) => {
  await login(page);
  const { key } = await disposableKey(page);
  await openEdit(page, key.id);
  const concurrent = await api(page, `/api/keys/${key.id}`, "PATCH", { label: "并发保存后的名称" }, key.version);
  expect(concurrent.status).toBe(200);
  await page.getByLabel("名称", { exact: true }).fill("旧页面不应覆盖");
  expectedRefusals.get(page)!.add(`412:/api/keys/${key.id}`);
  const refusal = page.waitForResponse(r => new URL(r.url()).pathname === `/api/keys/${key.id}` && r.request().method() === "PATCH");
  await page.getByRole("button", { name: "保存修改", exact: true }).click();
  expect((await refusal).status()).toBe(412);
  await expect(page.getByRole("alert")).toContainText("该密钥已被他人修改");
  expect((await api(page, `/api/keys/${key.id}`)).body.label).toBe("并发保存后的名称");
  await page.getByRole("button", { name: "重新载入最新配置", exact: true }).click();
  await expect(page.getByLabel("名称", { exact: true })).toHaveValue("并发保存后的名称");
});
