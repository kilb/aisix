import { expect, test, type Browser, type Locator, type Page } from "@playwright/test";
import { randomUUID } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

// 断言来自用户合同；不读取产品源码，不替换响应，不启动 fixture 后端。
// 退化会变红：费用混入次数/token；不限文案改成近义词；详情不能直达/刷新；
// 角色或额度仅改了界面；管理员能改所有者；重置未撤销旧 session；移除没有确认。
const session = resolve(requiredEnv("MEMBERS_QA_SESSION"));
const output = resolve(requiredEnv("MEMBERS_QA_OUTPUT"));
const state = readObject(join(session, "public.json"));
const auth = readObject(join(session, "local-auth.json"));
if (state.ready !== true || state.mockUpstream !== false) {
  throw new Error("MEMBERS_QA_SESSION must be a ready, real, isolated stack");
}
if (typeof state.tenantUrl !== "string") throw new Error("The isolated stack needs tenantUrl");
const baseUrl = state.tenantUrl.replace(/\/$/, "");
const origin = new URL(baseUrl);
if (!["127.0.0.1", "localhost"].includes(origin.hostname) || !["http:", "https:"].includes(origin.protocol)) {
  throw new Error("Member management E2E only accepts isolated loopback URLs");
}
const owner = credentials(auth.metricsOwner);

type JsonObject = Record<string, unknown>;
interface Credentials { email: string; password: string }
interface IsolatedMember extends Credentials { id: string; displayName: string }
interface ApiResult { status: number; body: unknown }
interface Member { id: string; email: string; role: string }
interface Budget { id: string; kind: string; period?: string; limit_micro_cny: number; target: { id: string } }

function requiredEnv(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is required`);
  return value;
}

function object(value: unknown): JsonObject {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("Expected a JSON object");
  return value as JsonObject;
}

function readObject(path: string): JsonObject {
  let value: unknown;
  try { value = JSON.parse(readFileSync(path, "utf8")); }
  catch { throw new Error("The isolated session file could not be read as JSON"); }
  return object(value);
}

function credentials(value: unknown): Credentials {
  const actor = object(value);
  if (typeof actor.email !== "string" || typeof actor.password !== "string") throw new Error("The isolated stack needs metricsOwner credentials");
  return { email: actor.email, password: actor.password };
}

// 错误不携带输入值，避免 Playwright 的失败日志回显密码。
async function fillSecret(field: Locator, value: string): Promise<void> {
  try { await field.fill(value); }
  catch { throw new Error("Could not fill a password control"); }
}

// 浏览器持有真实 cookie；只沿当前页面的同源通道调用真实 API。
async function api(page: Page, method: string, path: string, body?: JsonObject): Promise<ApiResult> {
  if (new URL(path, page.url()).origin !== origin.origin) throw new Error("Only same-origin API calls are allowed");
  try {
    return await page.evaluate(async ({ method, path, body }) => {
      const response = await fetch(path, {
        method, credentials: "include",
        ...(body === undefined ? {} : { headers: { "content-type": "application/json" }, body: JSON.stringify(body) }),
      });
      const raw = await response.text();
      return { status: response.status, body: raw ? JSON.parse(raw) as unknown : null };
    }, { method, path, body });
  } catch { throw new Error(`Same-origin ${method} ${path} did not complete`); }
}

async function settle(page: Page): Promise<void> {
  await page.evaluate(() => document.fonts.ready);
  await expect(page.getByText("载入中…", { exact: true })).toHaveCount(0);
}

async function login(page: Page, who: Credentials = owner): Promise<void> {
  await page.goto(baseUrl);
  await page.getByLabel("邮箱", { exact: true }).fill(who.email);
  await fillSecret(page.getByLabel("口令", { exact: true }), who.password);
  const response = waitForApi(page, "POST", "/api/login");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await response).status(), "real login").toBe(200);
  await settle(page);
  expect((await api(page, "GET", "/api/me")).status).toBe(200);
}

function waitForApi(page: Page, method: string, path: string) {
  return page.waitForResponse(response => new URL(response.url()).origin === origin.origin
    && new URL(response.url()).pathname === path && response.request().method() === method);
}

async function listMembers(page: Page): Promise<Member[]> {
  const response = await api(page, "GET", "/api/members");
  expect(response.status).toBe(200);
  const items = object(response.body).items;
  if (!Array.isArray(items)) throw new Error("Member response needs items");
  return items.map(value => {
    const item = object(value);
    if (typeof item.id !== "string" || typeof item.email !== "string" || typeof item.role !== "string") throw new Error("Member response needs explicit id, email and role");
    return { id: item.id, email: item.email, role: item.role };
  });
}

async function memberRole(page: Page, id: string): Promise<string | undefined> {
  return (await listMembers(page)).find(member => member.id === id)?.role;
}

async function createMember(page: Page): Promise<IsolatedMember> {
  const suffix = randomUUID();
  const member = {
    email: `members-qa-${suffix}@example.test`, password: `Members-${randomUUID()}!`,
    displayName: `隔离成员 ${suffix.slice(0, 8)}`,
  };
  const created = await api(page, "POST", "/api/members", {
    email: member.email, password: member.password, display_name: member.displayName, role: "member",
  });
  expect(created.status, "create a fresh isolated member").toBe(201);
  const stored = (await listMembers(page)).find(item => item.email === member.email);
  expect(stored !== undefined, "created member is persisted").toBe(true);
  if (!stored) throw new Error("Created member was not listed");
  expect(stored.role).toBe("member");
  mkdirSync(join(output, "isolated-members"), { recursive: true });
  writeFileSync(join(output, "isolated-members", stored.id + ".json"), JSON.stringify({ id: stored.id, email: member.email, display_name: member.displayName }));
  return { ...member, id: stored.id };
}

async function openDetail(page: Page, member: IsolatedMember): Promise<void> {
  await page.goto(baseUrl + "/#members");
  await page.getByRole("searchbox", { name: "搜索成员", exact: true }).fill(member.email);
  const entry = page.getByRole("link", { name: `${member.email} 的成员详情`, exact: true });
  await expect(entry).toHaveAttribute("href", `#members?member=${encodeURIComponent(member.id)}`);
  await entry.click();
  await expect(page).toHaveURL(baseUrl + "/#members?member=" + encodeURIComponent(member.id));
  await expect(page.getByRole("heading", { level: 1, name: member.displayName, exact: true })).toBeVisible();
}

async function choose(page: Page, label: string, option: string): Promise<void> {
  await page.getByRole("combobox", { name: label, exact: true }).click();
  await page.getByRole("option", { name: option, exact: true }).click();
}

async function memberBudgets(page: Page, id: string): Promise<Budget[]> {
  const response = await api(page, "GET", "/api/budgets");
  expect(response.status).toBe(200);
  const items = object(response.body).items;
  if (!Array.isArray(items)) throw new Error("Budget response needs items");
  return items.map(value => object(value)).filter(item => object(item.target).id === id).map(item => {
    if (typeof item.id !== "string" || typeof item.kind !== "string" || typeof item.limit_micro_cny !== "number") throw new Error("Budget response needs explicit id, kind and numeric limit_micro_cny");
    return { id: item.id, kind: item.kind, period: typeof item.period === "string" ? item.period : undefined, limit_micro_cny: item.limit_micro_cny, target: { id } };
  });
}

async function withActor(browser: Browser, who: Credentials, run: (page: Page) => Promise<void>): Promise<void> {
  const context = await browser.newContext();
  try { const page = await context.newPage(); await login(page, who); await run(page); }
  finally { await context.close(); }
}

test("成员列表只显示费用与精确不限文案，详情可以直达、刷新和返回", async ({ page }) => {
  await login(page);
  const member = await createMember(page);
  await page.goto(baseUrl + "/#members");
  await page.getByRole("searchbox", { name: "搜索成员", exact: true }).fill(member.email);
  const row = page.getByRole("row").filter({ hasText: member.email });
  await expect(row).toHaveCount(1);
  const table = page.getByRole("table").filter({ has: row });
  const headings = await table.getByRole("columnheader").allTextContents();
  const costColumn = headings.findIndex(heading => heading.trim() === "本月费用");
  expect(costColumn, "an explicit cost column exists").toBeGreaterThanOrEqual(0);
  await expect(row.getByRole("cell").nth(costColumn)).toHaveText("¥0.00");
  await expect(row).not.toContainText(/次调用|计费用量|token/i);
  await expect(row.getByText("不限制", { exact: true })).toBeVisible();
  await openDetail(page, member);
  for (const name of ["成员信息", "本月用量明细", "成员额度", "登录与访问"]) await expect(page.getByRole("region", { name, exact: true })).toBeVisible();
  const usage = page.getByRole("region", { name: "本月用量明细", exact: true });
  for (const name of ["已计费费用", "调用次数", "计费用量"]) await expect(usage.getByText(name, { exact: true })).toBeVisible();
  await expect(page.getByRole("region", { name: "成员信息", exact: true })).toContainText(member.email);
  await expect(page.getByRole("region", { name: "成员额度", exact: true }).getByText("不限制", { exact: true })).toBeVisible();
  const access = page.getByRole("region", { name: "登录与访问", exact: true });
  await expect(access.getByRole("switch")).toHaveCount(0);
  await expect(access.getByRole("button", { name: /^(启用|停用|禁用)(成员|账号)?$/ })).toHaveCount(0);
  const detailUrl = page.url();
  await page.reload();
  await expect(page.getByRole("heading", { level: 1, name: member.displayName, exact: true })).toBeVisible();
  expect(page.url()).toBe(detailUrl);
  await page.getByRole("button", { name: "返回成员列表", exact: true }).click();
  expect(new URL(page.url()).hash.split("?")[0]).toBe("#members");
  await page.getByRole("searchbox", { name: "搜索成员", exact: true }).fill(member.email);
  await expect(row).toBeVisible();
});

test("详情角色保存有确认且持久化，管理员不能修改隔离所有者", async ({ page, browser }) => {
  await login(page);
  const member = await createMember(page);
  await openDetail(page, member);
  await choose(page, "角色", "管理员");
  await page.getByRole("button", { name: "更改角色", exact: true }).click();
  await expect(page.getByRole("button", { name: "确认更改角色", exact: true })).toBeVisible();
  expect(await memberRole(page, member.id), "confirmation has not written the role").toBe("member");
  const changed = waitForApi(page, "PATCH", `/api/members/${member.id}`);
  await page.getByRole("button", { name: "确认更改角色", exact: true }).click();
  const response = await changed;
  expect(response.status()).toBe(200);
  expect(response.request().postDataJSON()).toEqual({ role: "admin" });
  expect(response.request().headers()["if-match"]).toBeUndefined();
  expect(await memberRole(page, member.id)).toBe("admin");
  await page.reload();
  await expect(page.getByRole("combobox", { name: "角色", exact: true })).toContainText("管理员");
  const isolatedOwner = await createMember(page);
  await openDetail(page, isolatedOwner);
  await choose(page, "角色", "所有者");
  await page.getByRole("button", { name: "更改角色", exact: true }).click();
  const promoted = waitForApi(page, "PATCH", `/api/members/${isolatedOwner.id}`);
  await page.getByRole("button", { name: "确认更改角色", exact: true }).click();
  expect((await promoted).status()).toBe(200);
  expect(await memberRole(page, isolatedOwner.id), "an owner can assign the owner role").toBe("owner");
  await withActor(browser, member, async adminPage => {
    for (const path of ["/api/members", "/api/members/usage", "/api/budgets"]) expect((await api(adminPage, "GET", path)).status, path).toBe(200);
    await openDetail(adminPage, isolatedOwner);
    for (const name of ["更改角色", "重置密码", "移除成员"]) await expect(adminPage.getByRole("button", { name, exact: true })).toHaveCount(0);
    expect((await api(adminPage, "PATCH", `/api/members/${isolatedOwner.id}`, { role: "member" })).status).toBe(403);
    expect((await api(adminPage, "POST", `/api/members/${isolatedOwner.id}/password`, { new_password: `Denied-${randomUUID()}!` })).status).toBe(403);
    expect((await api(adminPage, "DELETE", `/api/members/${isolatedOwner.id}`)).status).toBe(403);
  });
  expect(await memberRole(page, isolatedOwner.id)).toBe("owner");
});

test("详情成员额度通过真实 API 创建、调整、确认移除并恢复不限制", async ({ page }) => {
  await login(page);
  const member = await createMember(page);
  await openDetail(page, member);
  const region = page.getByRole("region", { name: "成员额度", exact: true });
  expect(await memberBudgets(page, member.id)).toEqual([]);
  await region.getByRole("button", { name: "设置成员额度", exact: true }).click();
  const create = page.getByRole("dialog", { name: "设置成员额度", exact: true });
  await choose(page, "额度类型", "按周期");
  await choose(page, "额度周期", "每月");
  await create.getByLabel("额度（元）", { exact: true }).fill("12.34");
  const created = waitForApi(page, "POST", "/api/budgets");
  await create.getByRole("button", { name: "保存额度", exact: true }).click();
  const createResponse = await created;
  expect(createResponse.status()).toBe(201);
  expect(createResponse.request().postDataJSON()).toEqual({ scope: "member", scope_ref: member.id, kind: "periodic", period: "month", limit_cny: "12.34" });
  const initial = await memberBudgets(page, member.id);
  expect(initial).toHaveLength(1);
  const budget = initial[0]!;
  expect(budget).toMatchObject({ kind: "periodic", period: "month", limit_micro_cny: 12_340_000 });
  await expect(region).toContainText("12.34");
  await region.getByRole("button", { name: "调整额度", exact: true }).click();
  const adjust = page.getByRole("dialog", { name: "调整成员额度", exact: true });
  await adjust.getByLabel("额度（元）", { exact: true }).fill("23.45");
  const adjusted = waitForApi(page, "PATCH", `/api/budgets/${budget.id}`);
  await adjust.getByRole("button", { name: "保存额度", exact: true }).click();
  const adjustResponse = await adjusted;
  expect(adjustResponse.status()).toBe(200);
  expect(adjustResponse.request().postDataJSON()).toEqual({ limit_cny: "23.45" });
  expect((await memberBudgets(page, member.id))[0]?.limit_micro_cny).toBe(23_450_000);
  await page.reload();
  await expect(region).toContainText("23.45");
  await region.getByRole("button", { name: "移除额度", exact: true }).click();
  await expect(region.getByRole("button", { name: "确认移除额度", exact: true })).toBeVisible();
  expect(await memberBudgets(page, member.id)).toHaveLength(1);
  const removed = waitForApi(page, "DELETE", `/api/budgets/${budget.id}`);
  await region.getByRole("button", { name: "确认移除额度", exact: true }).click();
  expect((await removed).status()).toBe(204);
  expect(await memberBudgets(page, member.id)).toEqual([]);
  await expect(region.getByText("不限制", { exact: true })).toBeVisible();
});

test("详情重置隔离成员密码撤销旧 session，旧密码失效而新密码可登录", async ({ page, browser }) => {
  await login(page);
  const member = await createMember(page);
  await withActor(browser, member, async memberPage => {
    await openDetail(page, member);
    await page.getByRole("button", { name: "重置密码", exact: true }).click();
    const dialog = page.getByRole("dialog", { name: "重置密码", exact: true });
    const newPassword = `Reset-${randomUUID()}!`;
    await fillSecret(dialog.getByLabel("新密码", { exact: true }), newPassword);
    await fillSecret(dialog.getByLabel("再次输入新密码", { exact: true }), newPassword);
    await dialog.getByRole("button", { name: "重置密码", exact: true }).click();
    await expect(dialog.getByRole("button", { name: "确认重置密码", exact: true })).toBeVisible();
    expect((await api(memberPage, "GET", "/api/me")).status, "confirmation has not revoked the session").toBe(200);
    const reset = waitForApi(page, "POST", `/api/members/${member.id}/password`);
    await dialog.getByRole("button", { name: "确认重置密码", exact: true }).click();
    const response = await reset;
    expect(response.status()).toBe(204);
    const sent = object(response.request().postDataJSON());
    expect(Object.keys(sent)).toEqual(["new_password"]);
    expect(sent.new_password === newPassword, "the submitted new password is exact").toBe(true);
    await expect(page.getByText("密码已重置。该成员需要使用新密码重新登录。", { exact: true })).toBeVisible();
    expect((await api(memberPage, "GET", "/api/me")).status, "the old real session was revoked").toBe(401);
    expect((await api(memberPage, "POST", "/api/login", { email: member.email, password: member.password })).status, "old password is refused").toBe(401);
    expect((await api(memberPage, "POST", "/api/login", { email: member.email, password: newPassword })).status, "new password signs in").toBe(200);
    expect((await api(memberPage, "GET", "/api/me")).status).toBe(200);
  });
  const own = (await listMembers(page)).find(member => member.email === owner.email);
  expect(own !== undefined).toBe(true);
  if (!own) throw new Error("The real owner must be in the member list");
  await page.goto(baseUrl + "/#members?member=" + encodeURIComponent(own.id));
  await expect(page.getByRole("region", { name: "登录与访问", exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "重置密码", exact: true })).toHaveCount(0);
});

test("移除成员可以取消且确认后真实移除，普通成员不能读取管理数据", async ({ page, browser }) => {
  await login(page);
  const member = await createMember(page);
  await withActor(browser, member, async memberPage => {
    for (const path of ["/api/members", "/api/members/usage"]) expect((await api(memberPage, "GET", path)).status, path).toBe(403);
  });
  await openDetail(page, member);
  const access = page.getByRole("region", { name: "登录与访问", exact: true });
  await access.getByRole("button", { name: "移除成员", exact: true }).click();
  await expect(access.getByRole("button", { name: "确认移除成员", exact: true })).toBeVisible();
  expect(await memberRole(page, member.id), "opening confirmation has not removed the member").toBe("member");
  await access.getByRole("button", { name: "取消", exact: true }).click();
  await expect(access.getByRole("button", { name: "确认移除成员", exact: true })).toHaveCount(0);
  expect(await memberRole(page, member.id), "cancel preserves the member").toBe("member");
  await access.getByRole("button", { name: "移除成员", exact: true }).click();
  const removed = waitForApi(page, "DELETE", `/api/members/${member.id}`);
  await access.getByRole("button", { name: "确认移除成员", exact: true }).click();
  expect((await removed).status()).toBe(204);
  expect((await listMembers(page)).some(item => item.id === member.id), "confirmed removal is persisted").toBe(false);
  expect(new URL(page.url()).hash.split("?")[0]).toBe("#members");
  await page.getByRole("searchbox", { name: "搜索成员", exact: true }).fill(member.email);
  await expect(page.getByRole("row").filter({ hasText: member.email })).toHaveCount(0);
});
