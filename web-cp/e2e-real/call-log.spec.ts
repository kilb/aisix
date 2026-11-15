import { expect, test, type Locator, type Page, type Response } from "@playwright/test";
import { mkdirSync, readFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { join, resolve } from "node:path";
import { chooseSelect } from "../e2e/select";

// 从真实保留记录选择场景；预期只来自用户可见合同与真实 API 响应。
const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
const output = resolve(process.env.REFINED_QA_OUTPUT!);
if (state.ready !== true || state.mockUpstream !== false) throw new Error("Call-log E2E requires a ready real isolated stack");
const base = state.adminUrl as string;
if (!["127.0.0.1", "localhost"].includes(new URL(base).hostname)) throw new Error("Only isolated loopback URLs are allowed");
interface ChargeRecord { event_id: string; status_code: number | null; error_class: string | null; error_message: string | null; attempt_index: number; is_final_attempt: boolean | null }
interface StoredRecord { event_id: string; request_id: string | null; event: Record<string, unknown> }
const requests = new WeakMap<Page, { method: string; path: string; search: string }[]>();
const errors = new WeakMap<Page, string[]>();
const consoleEntries = new WeakMap<Page, object[]>();
const expected404 = new WeakMap<Page, Set<string>>();

function listResponse(response: Response, eventId?: string): boolean {
  const url = new URL(response.url());
  return response.request().method() === "GET" && url.pathname === "/admin/pricing/charges" && (eventId === undefined ? !url.searchParams.has("event_id") : url.searchParams.get("event_id") === eventId);
}
function field(card: Locator, label: string): Locator {
  return card.locator("dl > div").filter({ has: card.page().locator("dt").getByText(label, { exact: true }) }).locator("dd");
}
async function settle(page: Page): Promise<void> {
  await page.evaluate(() => document.fonts.ready);
  await page.waitForLoadState("networkidle");
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
}
async function login(page: Page, target = base): Promise<void> {
  await page.goto(target);
  await page.getByLabel("邮箱", { exact: true }).fill(auth.operator.email);
  await page.getByLabel("口令", { exact: true }).fill(auth.operator.password);
  const answer = page.waitForResponse(response => new URL(response.url()).pathname === "/admin/login" && response.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await answer).status()).toBe(200);
  await settle(page);
}
test.beforeEach(async ({ page }) => {
  requests.set(page, []); errors.set(page, []); consoleEntries.set(page, []); expected404.set(page, new Set());
  page.on("console", message => {
    if (!["error", "warning"].includes(message.type())) return;
    consoleEntries.get(page)!.push({ type: message.type(), text: message.text(), location: message.location() });
    const location = message.location().url;
    const path = /^https?:/.test(location) ? new URL(location).pathname : "";
    if (message.type() !== "error" || message.text().includes("401") && /^\/admin\/(?:me|session)$/.test(path) || message.text().includes("404") && expected404.get(page)!.has(path)) return;
    errors.get(page)!.push(message.text());
  });
  page.on("pageerror", cause => errors.get(page)!.push(cause.message));
  page.on("request", request => { const url = new URL(request.url()); if (url.pathname.startsWith("/admin/")) requests.get(page)!.push({ method: request.method(), path: url.pathname, search: url.search }); });
  page.on("response", response => {
    const url = new URL(response.url());
    if (response.status() < 400 || !url.pathname.startsWith("/admin/")) return;
    if (response.status() === 401 && /^\/admin\/(?:me|session)$/.test(url.pathname)) return;
    if (response.status() === 404 && expected404.get(page)!.has(url.pathname)) return;
    errors.get(page)!.push(`HTTP ${response.status()} ${url.pathname}`);
  });
});
test.afterEach(async ({ page }) => {
  await test.info().attach("call-log-console", { body: JSON.stringify(consoleEntries.get(page)), contentType: "application/json" });
  await test.info().attach("call-log-request-ledger", { body: JSON.stringify(requests.get(page)), contentType: "application/json" });
  expect(errors.get(page), "uncaught application / unexpected HTTP errors").toEqual([]);
  expect(requests.get(page)!.filter(request => !["GET", "HEAD"].includes(request.method) && !/^\/admin\/(?:login|logout)$/.test(request.path)), "read-only log inspection must not mutate data").toEqual([]);
});

// 去掉详情入口、完整错误、复制全文、独立寻址或按需读取都会使此场景失败。
test("failed call opens its complete saved log, copies content and survives direct refresh", async ({ page, browser }) => {
  await login(page);
  const initial = page.waitForResponse(response => listResponse(response));
  await page.goto(`${base}/#calls?outcome=failed`);
  const answer = await initial;
  expect(answer.status()).toBe(200);
  const { items } = await answer.json() as { items: ChargeRecord[] };
  const sample = items.slice(0, 25).find(item => typeof item.error_message === "string" && item.error_message.length > 0);
  expect(sample, "Real retained failures must include a recorded error message").toBeDefined();
  const call = sample!;
  await settle(page);
  expect(requests.get(page)!.filter(request => /^\/admin\/usage\/events\//.test(request.path)), "the list must not prefetch every event").toEqual([]);
  const link = page.getByRole("link", { name: `查看调用日志 ${call.event_id}`, exact: true });
  const row = page.getByRole("row").filter({ has: link });
  await expect(row.locator(".call-error-summary")).toHaveText(call.error_message!);
  const detailPath = `/admin/usage/events/${encodeURIComponent(call.event_id)}`;
  const detailAnswer = page.waitForResponse(response => new URL(response.url()).pathname === detailPath);
  await link.click();
  const storedAnswer = await detailAnswer;
  expect(storedAnswer.status()).toBe(200);
  const stored = await storedAnswer.json() as StoredRecord;
  expect(stored.event_id).toBe(call.event_id);
  await expect(page.getByRole("heading", { name: "调用日志详情", exact: true })).toBeVisible();
  expect(await page.title()).toMatch(/AISIX/i);
  await expect(page.getByRole("table", { name: "计费明细", exact: true })).toBeHidden();
  const errorsCard = page.getByRole("region", { name: "调用结果与错误", exact: true });
  await expect(field(errorsCard, "HTTP 状态")).toHaveText(String(stored.event.status_code ?? call.status_code ?? "未记录"));
  await expect(field(errorsCard, "尝试编号（从 0 开始）")).toHaveText(stored.event.attempt_index === undefined || stored.event.attempt_index === null ? "未记录" : String(stored.event.attempt_index));
  const expectedMessage = String(stored.event.error_message ?? call.error_message);
  const message = page.getByRole("region", { name: "错误信息", exact: true });
  await expect(message.locator("pre")).toHaveText(expectedMessage);
  await message.getByRole("button", { name: "复制错误信息", exact: true }).click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe(expectedMessage);
  const json = page.getByRole("region", { name: "事件 JSON", exact: true });
  await expect(json.locator("pre")).toBeHidden();
  await json.locator("summary").click();
  await expect(json.locator("pre")).toHaveText(JSON.stringify(stored, null, 2));
  await json.getByRole("button", { name: "复制事件 JSON", exact: true }).click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe(JSON.stringify(stored, null, 2));
  const deepLink = page.url();
  expect(new URLSearchParams(deepLink.split("?")[1]).get("outcome")).toBe("failed");
  expect(new URLSearchParams(deepLink.split("?")[1]).get("detail")).toBe(call.event_id);
  const cursor = requests.get(page)!.length;
  await page.reload(); await settle(page);
  await expect(message.locator("pre")).toHaveText(expectedMessage);
  expect(page.url()).toBe(deepLink);
  const refreshedReads = requests.get(page)!.slice(cursor).filter(request => request.path === "/admin/pricing/charges");
  expect(refreshedReads.length).toBeGreaterThan(0);
  expect(refreshedReads.every(request => new URLSearchParams(request.search).get("event_id") === call.event_id), "direct detail refresh must not load the hidden unfiltered list").toBe(true);
  const isolated = await browser.newContext();
  const direct = await isolated.newPage();
  await login(direct, deepLink);
  await expect(direct.getByRole("heading", { name: "调用日志详情", exact: true })).toBeVisible();
  await expect(direct.getByRole("region", { name: "错误信息", exact: true }).locator("pre")).toHaveText(expectedMessage);
  await isolated.close();
  mkdirSync(join(output, "screenshots"), { recursive: true });
  for (const width of [390, 900, 1440]) {
    await page.setViewportSize({ width, height: 1000 });
    await expect(page.getByRole("link", { name: "返回调用记录", exact: true })).toBeVisible();
    const bounds = await page.evaluate(() => ({ viewport: document.documentElement.clientWidth, page: document.documentElement.scrollWidth, content: [...document.querySelectorAll(".call-log-text pre")].filter(element => element.getBoundingClientRect().width > 0).map(element => ({ client: element.clientWidth, scroll: element.scrollWidth })) }));
    expect(bounds.page, `page width at ${width}`).toBeLessThanOrEqual(bounds.viewport + 1);
    for (const block of bounds.content) expect(block.scroll, `long log wraps at ${width}`).toBeLessThanOrEqual(block.client + 1);
    await page.screenshot({ path: join(output, "screenshots", `call-log-${width}.png`) });
  }
  await page.getByRole("link", { name: "返回调用记录", exact: true }).click();
  await expect(page.getByRole("region", { name: "调用结果筛选", exact: true }).getByLabel("调用结果", { exact: true })).toHaveAttribute("data-value", "failed");
});

// 保留页码依赖的是现有真实记录集合，不能用替代响应制造分页。
test("returning from a log retains list filters, page size, page and local search", async ({ page }) => {
  await login(page);
  const initial = page.waitForResponse(response => listResponse(response));
  await page.goto(`${base}/#calls`);
  const answer = await initial;
  expect(answer.status()).toBe(200);
  const body = await answer.json() as { items: ChargeRecord[] };
  expect(body.items.length, "Real retained call records must contain at least 11 rows for pagination verification").toBeGreaterThan(10);
  const pager = page.getByRole("navigation", { name: "计费明细", exact: true });
  await chooseSelect(pager.getByLabel("计费明细每页条数", { exact: true }), "10");
  await pager.getByRole("button", { name: "下一页", exact: true }).click();
  await expect(pager).toContainText("第 2 页");
  const search = page.getByRole("searchbox", { name: "计费明细：在已加载记录中搜索", exact: true });
  const firstLink = page.getByRole("link", { name: /^查看调用日志 / }).first();
  const href = await firstLink.getAttribute("href");
  await firstLink.click();
  await expect(page.getByRole("heading", { name: "调用日志详情", exact: true })).toBeVisible();
  await page.getByRole("link", { name: "返回调用记录", exact: true }).click();
  await expect(pager).toContainText("第 2 页");
  await expect(pager.getByLabel("计费明细每页条数", { exact: true })).toHaveAttribute("data-value", "10");
  expect(await page.getByRole("link", { name: /^查看调用日志 / }).first().getAttribute("href")).toBe(href);
  const eventId = new URLSearchParams(href!.split("?")[1]).get("detail")!;
  await search.fill(eventId);
  await page.getByRole("link", { name: `查看调用日志 ${eventId}`, exact: true }).click();
  await expect(page.getByRole("heading", { name: "调用日志详情", exact: true })).toBeVisible();
  await page.goBack();
  await expect(search).toHaveValue(eventId);
  await expect(page.getByRole("link", { name: /^查看调用日志 / })).toHaveCount(1);
});

test("a missing event shows an explicit missing state and can return to the same list scope", async ({ page }) => {
  await login(page);
  const eventId = randomUUID();
  const detailPath = `/admin/usage/events/${eventId}`;
  expected404.get(page)!.add(detailPath);
  const eventAnswer = page.waitForResponse(response => new URL(response.url()).pathname === detailPath);
  const chargeAnswer = page.waitForResponse(response => listResponse(response, eventId));
  await page.goto(`${base}/#calls?outcome=failed&detail=${eventId}`);
  expect((await eventAnswer).status()).toBe(404);
  expect((await (await chargeAnswer).json()).items).toEqual([]);
  await expect(page.getByText("未找到此调用记录，记录可能已过保留期或事件编号不存在。", { exact: true })).toBeVisible();
  await page.getByRole("link", { name: "返回调用记录", exact: true }).click();
  expect(new URL(page.url()).hash).toBe("#calls?outcome=failed");
  await expect(page.getByRole("heading", { name: "调用记录", exact: true })).toBeVisible();
});
