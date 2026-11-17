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
interface ChargeRecord { status: string; charge_micro_cny: number | null; lines: { dimension: string; quantity: number; rate: string }[] | null; event_id: string; status_code: number | null; error_class: string | null; error_message: string | null; attempt_index: number; is_final_attempt: boolean | null }
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

// 把协议首帧改称客户端总耗时、交换字段或给缺失首字补零，都会失败。
test("下游响应阶段独立展示真实失败记录，不冒充客户端总耗时或首字", async ({ page }) => {
  await login(page);
  const read = (path: string) => page.evaluate(async url => {
    const response = await fetch(url, { credentials: "same-origin" });
    return { status: response.status, data: await response.json() as unknown };
  }, `${base}${path}`);
  const answer = await read("/admin/pricing/charges?outcome=failed&group_requests=true&limit=50");
  expect(answer.status).toBe(200);
  const { items } = answer.data as { items: ChargeRecord[] };
  expect(items.length, "隔离环境必须已有真实失败调用，不能伪造遥测行").toBeGreaterThan(0);
  let stored: StoredRecord | undefined;
  for (const call of items) {
    const response = await read(`/admin/usage/events/${encodeURIComponent(call.event_id)}`);
    if (response.status === 404) continue;
    expect(response.status).toBe(200);
    const candidate = response.data as StoredRecord;
    stored = candidate;
    break;
  }
  expect(stored, "真实失败调用必须能读取已保存事件").toBeDefined();
  const event = stored!.event;
  const duration = (value: unknown): string => {
    if (typeof value !== "number" || !Number.isFinite(value) || value < 0) return "—";
    return value < 1000 ? `${new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 }).format(value)} ms` : `${new Intl.NumberFormat("en-US", { maximumFractionDigits: 2 }).format(value / 1000)} s`;
  };
  await page.goto(`${base}/#calls?outcome=failed&detail=${encodeURIComponent(stored!.event_id)}`);
  await expect(page.getByRole("heading", { name: "调用日志详情", exact: true })).toBeVisible();
  await settle(page);
  const timing = page.getByRole("region", { name: "时间与性能", exact: true });
  await expect(field(timing, "下游响应阶段耗时")).toHaveText(duration(event.downstream_latency_ms));
  await expect(timing.locator("dt").getByText("客户端总耗时", { exact: true })).toHaveCount(0);
  const label = timing.locator("dt").getByText("下游响应阶段耗时", { exact: true });
  await expect(label).toHaveAttribute("title", /各接口.*处理起点/);
  await expect(label).toHaveAttribute("title", /首次下游协议输出.*元数据/);
  await expect(label).toHaveAttribute("title", /不代表客户端完整请求耗时/);
  await expect(field(timing, "首 Token 延迟")).toHaveText(duration(event.first_token_latency_ms));
  const policy = timing.locator("details").filter({ has: page.locator("summary").getByText("计时口径", { exact: true }) });
  const toggle = policy.locator("summary");
  const note = policy.locator("p");
  await expect(toggle).toBeVisible();
  await expect(note).toBeHidden();
  await toggle.focus();
  await page.keyboard.press("Shift+Tab");
  await page.keyboard.press("Tab");
  await expect(toggle).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(note).toBeVisible();
  await expect(note).toHaveText((await label.getAttribute("title"))!);
  await page.keyboard.press("Enter");
  await expect(note).toBeHidden();
  await test.info().attach("delivery-latency-safe-proof", { body: JSON.stringify({ event_id: stored!.event_id, downstream_latency_ms: event.downstream_latency_ms ?? null, first_token_latency_ms: event.first_token_latency_ms ?? null, upstream_latency_ms: event.upstream_latency_ms ?? null }), contentType: "application/json" });
  await label.scrollIntoViewIfNeeded();
  await page.screenshot({ path: join(output, "delivery-latency-details-desktop.png"), fullPage: false });
  await page.setViewportSize({ width: 390, height: 844 });
  await label.scrollIntoViewIfNeeded();
  await expect(label).toBeVisible();
  await expect(field(timing, "下游响应阶段耗时")).toHaveText(duration(event.downstream_latency_ms));
  await toggle.click();
  await expect(note).toBeVisible();
  await expect(note).toHaveText((await label.getAttribute("title"))!);
  await note.scrollIntoViewIfNeeded();
  await page.screenshot({ path: join(output, "delivery-latency-details-mobile.png"), fullPage: false });
});

const UPSTREAM_TIMING_FIELDS = [
  "http_send_started_ms", "response_headers_ms", "first_response_byte_ms",
  "first_protocol_frame_ms", "first_canonical_chunk_ms", "first_content_ms",
] as const;

/** 只按真实 API 原值核对原始偏移，缺省和非法值不能从其他时钟补齐。 */
async function expectUpstreamTiming(scope: Page | Locator, raw: unknown): Promise<void> {
  const region = scope.getByRole("region", { name: "上游 HTTP 阶段", exact: true });
  await expect(region).toHaveCount(1);
  await expect(region).toBeVisible();
  await expect(region.locator("[data-upstream-timing-field]")).toHaveCount(UPSTREAM_TIMING_FIELDS.length);
  const timing = raw !== null && typeof raw === "object" && !Array.isArray(raw) ? raw as Readonly<Record<string, unknown>> : null;
  for (const name of UPSTREAM_TIMING_FIELDS) {
    const slot = region.locator(`[data-upstream-timing-field="${name}"]`);
    await expect(slot).toHaveCount(1);
    const value = timing?.[name];
    const expected = typeof value === "number" && Number.isInteger(value) && value >= 0 && value <= 4_294_967_295
      ? `${new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 }).format(value)} ms` : "—";
    await expect(slot.locator("dd")).toHaveText(expected);
  }
  await expect(region).toContainText("各值均相对同一次上游尝试起点，按原始毫秒展示，不计算阶段差值");
  await expect(region).toContainText("不代表操作系统实际写包");
  await expect(region).toContainText("不代表客户端已收到或确认");
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
  await expectUpstreamTiming(page, stored.event.upstream_timing);
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
  await json.locator("summary").click();
  await expect(message.locator("pre")).toBeVisible();
  await expect(json.locator("pre")).toBeVisible();
  mkdirSync(join(output, "screenshots"), { recursive: true });
  for (const width of [390, 900, 1440]) {
    await page.setViewportSize({ width, height: 1000 });
    await expect(page.getByRole("link", { name: "返回调用记录", exact: true })).toBeVisible();
    const bounds = await page.evaluate(() => ({ viewport: document.documentElement.clientWidth, page: document.documentElement.scrollWidth, content: [...document.querySelectorAll(".call-log-text pre")].filter(element => element.getBoundingClientRect().width > 0).map(element => ({ client: element.clientWidth, scroll: element.scrollWidth })) }));
    expect(bounds.content).toHaveLength(2);
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


// 长错误与已计费明细可以属于不同真实调用；两种分支都必须先有证据再量布局。
test("long saved logs and billed quantity rows fit narrow detail pages without losing content", async ({ page }) => {
  const longErrorId = process.env.CALL_LOG_QA_LONG_EVENT_ID ?? state.callLogEvidence?.longErrorEventId;
  const billedId = process.env.CALL_LOG_QA_BILLED_EVENT_ID ?? state.callLogEvidence?.billedEventId;
  expect(typeof longErrorId === "string" && longErrorId.length > 0, "Prepare an actual saved failure with a long unbroken error field, then name its event ID").toBe(true);
  expect(typeof billedId === "string" && billedId.length > 0, "Prepare an actual billed call with nonempty quantity lines and a long numeric rate, then name its event ID").toBe(true);
  await login(page);
  for (const [kind, eventId] of [["long-error", longErrorId], ["billed", billedId]] as const) {
    const detailPath = `/admin/usage/events/${encodeURIComponent(eventId)}`;
    const eventRead = page.waitForResponse(response => new URL(response.url()).pathname === detailPath);
    const chargeRead = page.waitForResponse(response => listResponse(response, eventId));
    await page.goto(`${base}/#calls?detail=${encodeURIComponent(eventId)}`);
    const eventAnswer = await eventRead;
    const chargeAnswer = await chargeRead;
    expect(eventAnswer.status()).toBe(200);
    expect(chargeAnswer.status()).toBe(200);
    const stored = await eventAnswer.json() as StoredRecord;
    const priced = await chargeAnswer.json() as { items: ChargeRecord[] };
    expect(stored.event_id).toBe(eventId);
    await expectUpstreamTiming(page, stored.event.upstream_timing);
    await expect(page.getByRole("heading", { name: "调用日志详情", exact: true })).toBeVisible();
    const json = page.getByRole("region", { name: "事件 JSON", exact: true });
    await json.locator("summary").click();
    await expect(json.locator("pre")).toBeVisible();
    await expect(json.locator("pre")).toHaveText(JSON.stringify(stored, null, 2));
    if (kind === "long-error") {
      expect(typeof stored.event.error_message).toBe("string");
      expect(String(stored.event.error_message), "The real stored error must stress unbroken-line wrapping").toMatch(/[^\s]{160,}/u);
      await expect(page.getByRole("region", { name: "错误信息", exact: true }).locator("pre")).toHaveText(String(stored.event.error_message));
    } else {
      expect(priced.items).toHaveLength(1);
      const billed = priced.items[0]!;
      expect(billed.status).toBe("charged");
      expect(billed.charge_micro_cny).not.toBeNull();
      expect(billed.charge_micro_cny!).toBeGreaterThan(0);
      expect(Array.isArray(billed.lines)).toBe(true);
      expect(billed.lines!.length, "The real billed quantities branch must actually render").toBeGreaterThan(0);
      const longRateIndex = billed.lines!.findIndex(line => line.dimension !== "web_search" && line.quantity > 0 && /^\d+(?:\.\d+)?$/.test(line.rate) && line.rate.replace(/\D/g, "").length >= 12);
      expect(longRateIndex, "At least one actual billed Token rate must contain 12 digits to stress narrow quantity cells").toBeGreaterThanOrEqual(0);
      const quantities = page.getByRole("region", { name: `计费数量 ${eventId}`, exact: true });
      await expect(quantities.locator("table tbody tr")).toHaveCount(billed.lines!.length);
      const longLine = billed.lines![longRateIndex]!;
      const longRow = quantities.locator("table tbody tr").nth(longRateIndex);
      await expect(longRow.locator('td[data-label="基准价"]')).toHaveText(`$${longLine.rate} / 百万 tokens`);
      await expect(longRow.locator('td[data-label="数量"]')).toHaveText(new Intl.NumberFormat("en-US").format(longLine.quantity));
      for (const row of await quantities.locator("table tbody tr").all()) {
        await expect(row.locator("td")).toHaveCount(4);
        for (const label of ["维度", "数量", "基准价", "小计（美元）"]) await expect(row.locator(`td[data-label="${label}"]`)).not.toBeEmpty();
      }
    }
    for (const width of [390, 900, 1440]) {
      await page.setViewportSize({ width, height: 1000 });
      const errorBlock = page.getByRole("region", { name: "错误信息", exact: true }).locator("pre");
      if (kind === "long-error") await expect(errorBlock).toBeVisible();
      await expect(json.locator("pre")).toBeVisible();
      const table = page.getByRole("region", { name: `计费数量 ${eventId}`, exact: true }).locator("table");
      if (kind === "billed") await expect(table).toBeVisible();
      const bounds = await page.evaluate(() => {
        const visible = (element: Element) => element.getBoundingClientRect().width > 0;
        const rect = (element: Element) => { const r = element.getBoundingClientRect(); return { left: r.left, right: r.right, client: element.clientWidth, scroll: element.scrollWidth }; };
        return {
          viewport: document.documentElement.clientWidth, page: document.documentElement.scrollWidth,
          blocks: [...document.querySelectorAll(".call-log-text pre")].filter(visible).map(rect),
          tables: [...document.querySelectorAll(".call-log-page .charge-quantities-table")].filter(visible).map(rect),
          cells: [...document.querySelectorAll(".call-log-page .charge-quantities-table tbody td")].filter(visible).map(element => ({ ...rect(element), text: element.textContent })),
        };
      });
      const expectedBlocks = typeof stored.event.error_message === "string" && stored.event.error_message.length > 0 ? 2 : 1;
      expect(bounds.blocks).toHaveLength(expectedBlocks);
      expect(bounds.page, `${kind} page width at ${width}`).toBeLessThanOrEqual(bounds.viewport + 1);
      if (kind === "billed") {
        expect(bounds.tables).toHaveLength(1);
        expect(bounds.cells.length).toBeGreaterThanOrEqual(4);
      }
      for (const box of [...bounds.blocks, ...bounds.tables, ...bounds.cells]) {
        expect(box.left).toBeGreaterThanOrEqual(-1);
        expect(box.right).toBeLessThanOrEqual(bounds.viewport + 1);
        expect(box.scroll, `${kind} visible content fits at ${width}`).toBeLessThanOrEqual(box.client + 1);
      }
      mkdirSync(join(output, "screenshots"), { recursive: true });
      await page.screenshot({ path: join(output, "screenshots", `call-log-${kind}-${width}.png`), fullPage: true });
    }
  }
});

// 场景编号只能指向真实网关400/404请求已写入的事件，以及未采集阶段的历史事件。
test("real rejected calls show their recorded request phases while legacy events remain unknown", async ({ page }) => {
  const phase400 = process.env.CALL_LOG_QA_PHASE_400_EVENT_ID ?? state.callLogEvidence?.phase400EventId;
  const phase404 = process.env.CALL_LOG_QA_PHASE_404_EVENT_ID ?? state.callLogEvidence?.phase404EventId;
  const legacy = process.env.CALL_LOG_QA_LEGACY_EVENT_ID ?? state.callLogEvidence?.legacyEventId;
  for (const [name, eventId] of [["actual fully read JSON request rejected with 400", phase400], ["actual fully read unknown-model 404", phase404], ["legacy event without request timing", legacy]]) {
    expect(typeof eventId === "string" && eventId.length > 0, `Prepare the real ${name} event and supply its ID`).toBe(true);
  }
  await login(page);
  const phaseKeys = ["body_read_ms", "json_decode_ms", "pre_handler_ms", "request_body_bytes", "request_body_complete"] as const;
  const duration = (value: number) => `${new Intl.NumberFormat("en-US", { maximumFractionDigits: 2 }).format(value < 1000 ? value : value / 1000)} ${value < 1000 ? "ms" : "s"}`;
  for (const [status, eventId] of [[400, phase400], [404, phase404], [null, legacy]] as const) {
    const eventRead = page.waitForResponse(response => new URL(response.url()).pathname === `/admin/usage/events/${encodeURIComponent(eventId)}`);
    const chargeRead = page.waitForResponse(response => listResponse(response, eventId));
    await page.goto(`${base}/#calls?detail=${encodeURIComponent(eventId)}`);
    const eventAnswer = await eventRead;
    const chargeAnswer = await chargeRead;
    expect(eventAnswer.status()).toBe(200);
    expect(chargeAnswer.status()).toBe(200);
    const stored = await eventAnswer.json() as StoredRecord;
    const priced = await chargeAnswer.json() as { items: (ChargeRecord & { request_timing?: unknown })[] };
    expect(stored.event_id).toBe(eventId);
    expect(priced.items).toHaveLength(1);
    const stage = page.getByRole("region", { name: "请求入口阶段", exact: true });
    await expect(stage).toBeVisible();
    const shown = (key: string) => stage.locator(`[data-request-timing-field="${key}"] dd`);
    if (status === null) {
      expect(stored.event.request_timing == null, "the actual historical event must lack phase measurements").toBe(true);
      expect(priced.items[0]!.request_timing == null).toBe(true);
      for (const key of phaseKeys) await expect(shown(key)).toHaveText("未记录");
      await expect(stage.getByText("此记录未采集请求入口阶段，历史缺失不按零显示。", { exact: true })).toBeVisible();
      continue;
    }
    expect(stored.event.status_code).toBe(status);
    expect(typeof stored.event.request_timing).toBe("object");
    expect(stored.event.request_timing).not.toBeNull();
    const timing = stored.event.request_timing as Record<string, unknown>;
    for (const key of phaseKeys) expect(Object.hasOwn(timing, key), `the actual ${status} event must explicitly record ${key}`).toBe(true);
    for (const key of ["body_read_ms", "json_decode_ms", "pre_handler_ms", "request_body_bytes"]) {
      expect(Number.isFinite(timing[key])).toBe(true);
      expect(timing[key] as number).toBeGreaterThanOrEqual(0);
    }
    expect(timing.request_body_bytes as number).toBeGreaterThan(0);
    expect(timing.request_body_complete, "these two real requests fully sent their body before the rejection").toBe(true);
    expect(priced.items[0]!.request_timing, "the existing charge projection must preserve the same phase object").toEqual(timing);
    for (const key of ["body_read_ms", "json_decode_ms", "pre_handler_ms"]) await expect(shown(key)).toHaveText(duration(timing[key] as number));
    await expect(shown("request_body_bytes")).toHaveText(`已读取 ${new Intl.NumberFormat("en-US").format(timing.request_body_bytes as number)} 字节`);
    await expect(shown("request_body_complete")).toHaveText("已读至结束");
    await expect(stage).toContainText("阶段可能重叠，不能直接相加");
    await expect(stage).toContainText("首 Token 延迟从请求到达起计，包含上传和处理时间");
    await expect(stage).toContainText("上游首帧延迟只对应本次上游尝试");
  }
});
