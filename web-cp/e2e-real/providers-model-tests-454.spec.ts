import { expect, test, type Page } from "@playwright/test";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

// 真实隔离控制面中预先保存账号/映射/通配直连模型；不替换网络，不写探测结果。
const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
interface Named { id: string; name: string }
interface Fixture { accounts: [Named, Named, Named]; directModels: [Named, Named, Named] }
const fixture: Fixture = JSON.parse(readFileSync(process.env.PROVIDERS_TESTS_QA_FIXTURE!, "utf8"));
if (state.ready !== true || state.mockUpstream !== false) throw new Error("A ready real isolated stack is required");
if (!["127.0.0.1", "localhost"].includes(new URL(state.adminUrl).hostname)) throw new Error("Only isolated loopback URLs are allowed");

async function login(page: Page): Promise<void> {
  await page.goto(state.adminUrl);
  await page.getByLabel("邮箱", { exact: true }).fill(auth.operator.email);
  await page.getByLabel("口令", { exact: true }).fill(auth.operator.password);
  const response = page.waitForResponse(reply => new URL(reply.url()).pathname === "/admin/login" && reply.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await response).status()).toBe(200);
  await page.waitForLoadState("networkidle");
}

async function get(page: Page, path: string): Promise<any> {
  return page.evaluate(async target => {
    const response = await fetch(target, { credentials: "include" });
    if (!response.ok) throw new Error(`Real API ${response.status}`);
    return response.json();
  }, path);
}

test("#96 saved channel mappings are counted once per account and manual direct models stay unmapped", async ({ page }) => {
  const errors: string[] = [];
  const reads: string[] = [];
  page.on("pageerror", error => errors.push(error.message));
  await login(page);
  const models = await get(page, "/admin/public-models");
  const actual = fixture.accounts.map(account => models.items.filter((model: any) => model.retired_at === null && model.served_by.some((row: any) => row.provider_key_id === account.id)).length);
  expect(actual, "真实准备场景应为两个、一条、无公共映射；包含禁用映射").toEqual([2, 1, 0]);
  const direct = await get(page, "/admin/resources/models");
  expect(direct.items.some((model: any) => model.doc.provider_key_id === fixture.accounts[2].id), "零映射账号必须同时拥有手工直连模型，才能揭露误算").toBe(true);
  page.on("request", request => {
    if (request.method() === "GET") reads.push(new URL(request.url()).pathname);
  });
  await page.goto(`${state.adminUrl}/#providers`);
  const table = page.getByRole("table", { name: "上游账号", exact: true });
  await expect(table.getByRole("columnheader", { name: "映射模型数量", exact: true })).toBeVisible();
  for (const [index, account] of fixture.accounts.entries()) {
    await page.getByRole("searchbox", { name: "搜索上游账号", exact: true }).fill(account.name);
    const row = table.getByRole("row").filter({ has: page.getByRole("cell", { name: account.name, exact: true }) });
    const cell = row.getByRole("cell").nth(4);
    await expect(cell).toHaveText(index === 2 ? "0未映射" : String([2, 1][index]));
  }
  expect(reads.filter(path => path === "/admin/public-models")).toHaveLength(1);
  expect(reads.filter(path => /\/provider-keys\/[^/]+\/channel-models$/.test(path))).toEqual([]);
  expect(errors).toEqual([]);
});

test("#97 per-row tests and remaining tests persist real offline outcomes and preserve earlier records", async ({ page }) => {
  const errors: string[] = [];
  const posts: string[] = [];
  page.on("pageerror", error => errors.push(error.message));
  page.on("request", request => {
    const path = new URL(request.url()).pathname;
    if (request.method() === "POST" && /^\/admin\/models\/[^/]+\/probe$/.test(path)) posts.push(path);
  });
  await login(page);
  const resources = await get(page, "/admin/resources/models");
  expect(resources.items.filter((row: any) => row.doc.provider_key_id && !String(row.doc.model_name ?? "").includes("*")), "整个隔离清单均须使用调用前拒绝的通配上游名，防止批量按钮触及其它付费目标").toEqual([]);
  for (const target of fixture.directModels) {
    const model = resources.items.find((row: any) => row.id === target.id);
    expect(model, "真实直连模型必须存在").toBeDefined();
    expect(model.doc.model_name, "只允许通配模型的实际调用前拒绝边界，禁止任何付费上游派发").toContain("*");
  }
  const initial = await get(page, "/admin/model-probes?latest=true&limit=100");
  expect(initial.items.filter((row: any) => fixture.directModels.some(model => model.id === row.model_id))).toEqual([]);
  await page.goto(`${state.adminUrl}/#model-tests`);
  await expect(page.getByRole("button", { name: "测试剩余模型（3）", exact: true })).toBeEnabled();
  const table = page.getByRole("table").filter({ has: page.getByRole("columnheader", { name: "基础性能", exact: true }) });
  await expect(table.getByRole("columnheader", { name: "操作", exact: true })).toBeVisible();
  const [first, second, third] = fixture.directModels;
  const one = page.waitForResponse(response => new URL(response.url()).pathname === `/admin/models/${first.id}/probe` && response.request().method() === "POST");
  await page.getByRole("button", { name: `测试模型 ${first.name}`, exact: true }).click();
  expect((await one).status()).toBe(200);
  await expect(page.getByRole("button", { name: "测试剩余模型（2）", exact: true })).toBeEnabled();
  expect(posts).toEqual([`/admin/models/${first.id}/probe`]);
  const before = await get(page, `/admin/model-probes?model_id=${first.id}&latest=true&limit=100`);
  expect(before.items).toHaveLength(1);
  expect(before.items[0].state).toBe("completed");
  expect(before.items[0].result.status).toBe("unsupported");
  expect(Object.hasOwn(before.items[0].result, "checked_at")).toBe(true);
  const firstRow = table.getByRole("row").filter({ has: page.getByRole("button", { name: `测试模型 ${first.name}`, exact: true }) });
  const earlierTime = await firstRow.locator("time").getAttribute("datetime");
  expect(earlierTime).toBe(before.items[0].result.checked_at);
  await page.getByRole("button", { name: "测试剩余模型（2）", exact: true }).click();
  await expect(page.getByRole("button", { name: "测试剩余模型（0）", exact: true })).toBeDisabled();
  expect(posts.slice(1).sort()).toEqual([second, third].map(model => `/admin/models/${model.id}/probe`).sort());
  await expect(firstRow.locator("time")).toHaveAttribute("datetime", earlierTime!);
  const after = await get(page, `/admin/model-probes?model_id=${first.id}&latest=true&limit=100`);
  expect(after.items[0].id).toBe(before.items[0].id);
  expect(after.items[0].result).toEqual(before.items[0].result);
  await page.reload();
  await expect(page.getByRole("button", { name: "测试剩余模型（0）", exact: true })).toBeDisabled();
  for (const target of fixture.directModels) await expect(page.getByRole("button", { name: `测试模型 ${target.name}`, exact: true })).toBeEnabled();
  expect(posts).toHaveLength(3);
  expect(errors).toEqual([]);
});
